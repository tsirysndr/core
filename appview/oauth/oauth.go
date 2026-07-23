package oauth

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"sync"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/bluesky-social/indigo/atproto/auth/oauth"
	"github.com/bluesky-social/indigo/atproto/syntax"
	xrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/gorilla/sessions"
	"github.com/hashicorp/golang-lru/v2/expirable"
	"github.com/posthog/posthog-go"
	"golang.org/x/sync/singleflight"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/hostutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/rbac"
	"tangled.org/core/xrpc/serviceauth"
)

const (
	sessionCacheSize = 10000
	sessionCacheTTL  = time.Hour
)

type KnotMembership interface {
	IsKnotMember(ctx context.Context, host, userDid string) bool
	InvalidateMembers(host string)
}

type OAuth struct {
	ClientApp  *oauth.ClientApp
	SessStore  *sessions.CookieStore
	Config     *config.Config
	JwksUri    string
	ClientName string
	ClientUri  string
	Posthog    posthog.Client
	Db         *db.DB
	Enforcer   *rbac.Enforcer
	Acl        KnotMembership
	IdResolver *idresolver.Resolver
	Logger     *slog.Logger

	appPasswordSession   *AppPasswordSession
	appPasswordSessionMu sync.Mutex

	sessionCache *expirable.LRU[string, *oauth.ClientSession]
	sessionSF    singleflight.Group
}

func sessionCacheKey(did syntax.DID, sessionId string) string {
	return string(did) + ":" + sessionId
}

func (o *OAuth) resumeSession(ctx context.Context, did syntax.DID, sessionId string) (*oauth.ClientSession, error) {
	key := sessionCacheKey(did, sessionId)
	if v, ok := o.sessionCache.Get(key); ok {
		return v, nil
	}
	v, err, _ := o.sessionSF.Do(key, func() (any, error) {
		if v, ok := o.sessionCache.Get(key); ok {
			return v, nil
		}
		sess, err := o.ClientApp.ResumeSession(ctx, did, sessionId)
		if err != nil {
			return nil, err
		}
		o.sessionCache.Add(key, sess)
		return sess, nil
	})
	if err != nil {
		return nil, err
	}
	return v.(*oauth.ClientSession), nil
}

func (o *OAuth) EvictSession(did syntax.DID, sessionId string) {
	o.sessionCache.Remove(sessionCacheKey(did, sessionId))
}

func (o *OAuth) HandlePermanentAuthErr(ctx context.Context, did syntax.DID, sessionId string, err error) bool {
	if !IsPermanentAuthErr(err) {
		return false
	}
	o.EvictSession(did, sessionId)
	if logoutErr := o.ClientApp.Logout(ctx, did, sessionId); logoutErr != nil {
		o.Logger.Warn("store logout after permanent auth error failed", "did", did, "err", logoutErr)
	}
	return true
}

func New(config *config.Config, ph posthog.Client, db *db.DB, enforcer *rbac.Enforcer, acl KnotMembership, res *idresolver.Resolver, logger *slog.Logger) (*OAuth, error) {
	var oauthConfig oauth.ClientConfig
	clientUri := config.Core.BaseUrl()
	callbackUri := clientUri + "/oauth/callback"
	if config.Core.Dev {
		if config.Core.Hostname() == "localhost" {
			logger.Warn("dev OAuth requires a loopback IP host; use 127.0.0.1 instead of 'localhost'", "host", config.Core.AppviewHost)
		}
		oauthConfig = oauth.NewLocalhostConfig(callbackUri, TangledScopes)
	} else {
		clientId := fmt.Sprintf("%s/oauth/client-metadata.json", clientUri)
		oauthConfig = oauth.NewPublicConfig(clientId, callbackUri, TangledScopes)
	}

	// configure client secret
	priv, err := atcrypto.ParsePrivateMultibase(config.OAuth.ClientSecret)
	if err != nil {
		return nil, err
	}
	if err := oauthConfig.SetClientSecret(priv, config.OAuth.ClientKid); err != nil {
		return nil, err
	}

	jwksUri := clientUri + "/oauth/jwks.json"

	authStore, err := NewRedisStore(&RedisStoreConfig{
		RedisURL:                  config.Redis.ToURL(),
		SessionExpiryDuration:     time.Hour * 24 * 90,
		SessionInactivityDuration: time.Hour * 24 * 14,
		AuthRequestExpiryDuration: time.Minute * 30,
	})
	if err != nil {
		return nil, err
	}

	sessStore := sessions.NewCookieStore([]byte(config.Core.CookieSecret))
	sessStore.Options.SameSite = http.SameSiteLaxMode
	sessStore.Options.HttpOnly = true
	sessStore.Options.Secure = !config.Core.Dev

	clientApp := oauth.NewClientApp(&oauthConfig, authStore)
	clientApp.Dir = res.Directory()
	// allow non-public transports in dev mode
	if config.Core.Dev {
		clientApp.Resolver.Client.Transport = http.DefaultTransport
	}

	clientName := config.Core.AppviewName

	logger.Info("oauth setup successfully", "IsConfidential", clientApp.Config.IsConfidential())
	return &OAuth{
		ClientApp:    clientApp,
		Config:       config,
		SessStore:    sessStore,
		JwksUri:      jwksUri,
		ClientName:   clientName,
		ClientUri:    clientUri,
		Posthog:      ph,
		Db:           db,
		Enforcer:     enforcer,
		Acl:          acl,
		IdResolver:   res,
		Logger:       logger,
		sessionCache: expirable.NewLRU[string, *oauth.ClientSession](sessionCacheSize, nil, sessionCacheTTL),
	}, nil
}

func (o *OAuth) SaveSession(w http.ResponseWriter, r *http.Request, sessData *oauth.ClientSessionData) error {
	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil {
		o.Logger.Warn("failed to decode existing session cookie, will create new", "err", err)
	}

	userSession.Values[SessionDid] = sessData.AccountDID.String()
	userSession.Values[SessionPds] = sessData.HostURL
	userSession.Values[SessionId] = sessData.SessionID
	userSession.Values[SessionAuthenticated] = true

	if err := userSession.Save(r, w); err != nil {
		return err
	}

	handle := ""
	resolved, err := o.IdResolver.ResolveIdent(r.Context(), sessData.AccountDID.String())
	if err == nil && resolved.Handle.String() != "" {
		handle = resolved.Handle.String()
	}

	registry := o.GetAccounts(r)
	if err := registry.AddAccount(sessData.AccountDID.String(), handle, sessData.SessionID); err != nil {
		return err
	}
	return o.saveAccounts(w, r, registry)
}

func (o *OAuth) ResumeSession(r *http.Request) (*oauth.ClientSession, error) {
	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil {
		return nil, fmt.Errorf("error getting user session: %w", err)
	}
	if userSession.IsNew {
		return nil, fmt.Errorf("no session available for user")
	}

	d := userSession.Values[SessionDid].(string)
	sessDid, err := syntax.ParseDID(d)
	if err != nil {
		return nil, fmt.Errorf("malformed DID in session cookie '%s': %w", d, err)
	}

	sessId := userSession.Values[SessionId].(string)

	clientSess, err := o.resumeSession(r.Context(), sessDid, sessId)
	if err != nil {
		return nil, fmt.Errorf("failed to resume session: %w", err)
	}

	return clientSess, nil
}

func (o *OAuth) DeleteSession(w http.ResponseWriter, r *http.Request) error {
	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil {
		return fmt.Errorf("error getting user session: %w", err)
	}
	if userSession.IsNew {
		return fmt.Errorf("no session available for user")
	}

	d := userSession.Values[SessionDid].(string)
	sessDid, err := syntax.ParseDID(d)
	if err != nil {
		return fmt.Errorf("malformed DID in session cookie '%s': %w", d, err)
	}

	sessId := userSession.Values[SessionId].(string)

	o.EvictSession(sessDid, sessId)

	// delete the session
	err1 := o.ClientApp.Logout(r.Context(), sessDid, sessId)
	if err1 != nil {
		err1 = fmt.Errorf("failed to logout: %w", err1)
	}
	o.EvictSession(sessDid, sessId)

	// remove the cookie
	userSession.Options.MaxAge = -1
	err2 := o.SessStore.Save(r, w, userSession)
	if err2 != nil {
		err2 = fmt.Errorf("failed to save into session store: %w", err2)
	}

	return errors.Join(err1, err2)
}

func (o *OAuth) SwitchAccount(w http.ResponseWriter, r *http.Request, targetDid string) error {
	registry := o.GetAccounts(r)
	account := registry.FindAccount(targetDid)
	if account == nil {
		return fmt.Errorf("account not found in registry: %s", targetDid)
	}

	did, err := syntax.ParseDID(targetDid)
	if err != nil {
		return fmt.Errorf("invalid DID: %w", err)
	}

	sess, err := o.resumeSession(r.Context(), did, account.SessionId)
	if err != nil {
		registry.RemoveAccount(targetDid)
		_ = o.saveAccounts(w, r, registry)
		return fmt.Errorf("session expired for account: %w", err)
	}

	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil {
		return err
	}

	userSession.Values[SessionDid] = sess.Data.AccountDID.String()
	userSession.Values[SessionPds] = sess.Data.HostURL
	userSession.Values[SessionId] = sess.Data.SessionID
	userSession.Values[SessionAuthenticated] = true

	return userSession.Save(r, w)
}

func (o *OAuth) RemoveAccount(w http.ResponseWriter, r *http.Request, targetDid string) error {
	registry := o.GetAccounts(r)
	account := registry.FindAccount(targetDid)
	if account == nil {
		return nil
	}

	did, err := syntax.ParseDID(targetDid)
	if err == nil {
		o.EvictSession(did, account.SessionId)
		_ = o.ClientApp.Logout(r.Context(), did, account.SessionId)
		o.EvictSession(did, account.SessionId)
	}

	registry.RemoveAccount(targetDid)
	return o.saveAccounts(w, r, registry)
}

func (o *OAuth) GetDid(r *http.Request) string {
	if u := o.GetMultiAccountUser(r); u != nil {
		return u.Did
	}

	return ""
}

func (o *OAuth) GetDidFromCookie(r *http.Request) syntax.DID {
	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil || userSession.IsNew {
		return ""
	}
	d, ok := userSession.Values[SessionDid].(string)
	if !ok {
		return ""
	}
	parsed, err := syntax.ParseDID(d)
	if err != nil {
		return ""
	}
	return parsed
}

func (o *OAuth) GetSessIdFromCookie(r *http.Request) string {
	userSession, err := o.SessStore.Get(r, SessionName)
	if err != nil || userSession.IsNew {
		return ""
	}
	s, ok := userSession.Values[SessionId].(string)
	if !ok {
		return ""
	}
	return s
}

func (o *OAuth) AuthorizedClient(r *http.Request) (*atclient.APIClient, error) {
	session, err := o.ResumeSession(r)
	if err != nil {
		return nil, fmt.Errorf("error getting session: %w", err)
	}
	return session.APIClient(), nil
}

// this is a higher level abstraction on ServerGetServiceAuth
type ServiceClientOpts struct {
	service string
	exp     int64
	lxm     string
	dev     bool
	timeout time.Duration
}

type ServiceClientOpt func(*ServiceClientOpts)

func DefaultServiceClientOpts() ServiceClientOpts {
	return ServiceClientOpts{
		timeout: time.Second * 5,
	}
}

func WithService(service string) ServiceClientOpt {
	return func(s *ServiceClientOpts) {
		s.service = service
	}
}

// Specify the Duration in seconds for the expiry of this token
//
// The time of expiry is calculated as time.Now().Unix() + exp
func WithExp(exp int64) ServiceClientOpt {
	return func(s *ServiceClientOpts) {
		s.exp = time.Now().Unix() + exp
	}
}

func WithLxm(lxm string) ServiceClientOpt {
	return func(s *ServiceClientOpts) {
		s.lxm = lxm
	}
}

func WithDev(dev bool) ServiceClientOpt {
	return func(s *ServiceClientOpts) {
		s.dev = dev
	}
}

func WithTimeout(timeout time.Duration) ServiceClientOpt {
	return func(s *ServiceClientOpts) {
		s.timeout = timeout
	}
}

func (s *ServiceClientOpts) Audience() string {
	return serviceauth.DidWeb(s.service).String()
}

func (s *ServiceClientOpts) Host() string {
	scheme := "https://"
	if s.dev {
		scheme = "http://"
	}

	return scheme + s.service
}

func (o *OAuth) ServiceClient(r *http.Request, os ...ServiceClientOpt) (*xrpc.Client, error) {
	client, err := o.AuthorizedClient(r)
	if err != nil {
		return nil, err
	}

	opts := DefaultServiceClientOpts()
	for _, o := range os {
		o(&opts)
	}

	// force expiry to atleast 60 seconds in the future
	sixty := time.Now().Unix() + 60
	if opts.exp < sixty {
		opts.exp = sixty
	}

	resp, err := comatproto.ServerGetServiceAuth(r.Context(), client, opts.Audience(), opts.exp, opts.lxm)
	if err != nil {
		return nil, err
	}

	return &xrpc.Client{
		Auth: &xrpc.AuthInfo{
			AccessJwt: resp.Token,
		},
		Host: opts.Host(),
		Client: &http.Client{
			Timeout: opts.timeout,
		},
	}, nil
}

func (o *OAuth) SpindleServiceClient(r *http.Request, spindle, lxm string) (*xrpc.Client, error) {
	hostname, noTLS, err := hostutil.ParseHostname(spindle)
	if err != nil {
		return nil, err
	}
	return o.ServiceClient(
		r,
		WithService(hostname),
		WithLxm(lxm),
		WithDev(noTLS),
		WithTimeout(time.Second*30),
	)
}

func (o *OAuth) StartElevatedAuthFlow(ctx context.Context, w http.ResponseWriter, r *http.Request, did string, extraScopes []string, returnURL string) (string, error) {
	parsedDid, err := syntax.ParseDID(did)
	if err != nil {
		return "", fmt.Errorf("invalid DID: %w", err)
	}

	ident, err := o.ClientApp.Dir.Lookup(ctx, parsedDid.AtIdentifier())
	if err != nil {
		return "", fmt.Errorf("failed to resolve DID (%s): %w", did, err)
	}

	host := ident.PDSEndpoint()
	if host == "" {
		return "", fmt.Errorf("identity does not link to an atproto host (PDS)")
	}

	authserverURL, err := o.ClientApp.Resolver.ResolveAuthServerURL(ctx, host)
	if err != nil {
		return "", fmt.Errorf("resolving auth server: %w", err)
	}

	authserverMeta, err := o.ClientApp.Resolver.ResolveAuthServerMetadata(ctx, authserverURL)
	if err != nil {
		return "", fmt.Errorf("fetching auth server metadata: %w", err)
	}

	scopes := make([]string, 0, len(TangledScopes)+len(extraScopes))
	scopes = append(scopes, TangledScopes...)
	scopes = append(scopes, extraScopes...)

	loginHint := did
	if ident.Handle != "" && !ident.Handle.IsInvalidHandle() {
		loginHint = ident.Handle.String()
	}

	info, err := o.ClientApp.SendAuthRequest(ctx, authserverMeta, scopes, loginHint)
	if err != nil {
		return "", fmt.Errorf("auth request failed: %w", err)
	}

	info.AccountDID = &parsedDid
	o.ClientApp.Store.SaveAuthRequestInfo(ctx, *info)

	if err := o.SetAuthReturn(w, r, returnURL); err != nil {
		return "", fmt.Errorf("failed to set auth return: %w", err)
	}

	redirectURL := fmt.Sprintf("%s?client_id=%s&request_uri=%s",
		authserverMeta.AuthorizationEndpoint,
		url.QueryEscape(o.ClientApp.Config.ClientID),
		url.QueryEscape(info.RequestURI),
	)

	return redirectURL, nil
}
