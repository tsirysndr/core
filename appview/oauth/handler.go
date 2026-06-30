package oauth

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/auth/oauth"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	xrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
	"github.com/posthog/posthog-go"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/consts"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/tid"
)

const knotAdminTimeout = 30 * time.Second

func (o *OAuth) Router() http.Handler {
	r := chi.NewRouter()

	r.Get("/oauth/client-metadata.json", o.clientMetadata)
	r.Get("/oauth/jwks.json", o.jwks)
	r.Get("/oauth/callback", o.callback)
	return r
}

func (o *OAuth) clientMetadata(w http.ResponseWriter, r *http.Request) {
	doc := o.ClientApp.Config.ClientMetadata()
	doc.JWKSURI = &o.JwksUri
	doc.ClientName = &o.ClientName
	doc.ClientURI = &o.ClientUri
	doc.Scope = doc.Scope + " identity:handle"

	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(doc); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
}

func (o *OAuth) jwks(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	body := o.ClientApp.Config.PublicJWKS()
	if err := json.NewEncoder(w).Encode(body); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
}

func (o *OAuth) callback(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	l := o.Logger.With("query", r.URL.Query())

	redirectURL := o.GetAuthReturn(r)
	_ = o.ClearAuthReturn(w, r)

	sessData, err := o.ClientApp.ProcessCallback(ctx, r.URL.Query())
	if err != nil {
		var callbackErr *oauth.AuthRequestCallbackError
		if errors.As(err, &callbackErr) {
			l.Debug("callback error", "err", callbackErr)
			http.Redirect(w, r, fmt.Sprintf("/login?error=%s", callbackErr.ErrorCode), http.StatusFound)
			return
		}
		l.Error("failed to process callback", "err", err)
		http.Redirect(w, r, "/login?error=oauth", http.StatusFound)
		return
	}

	if err := o.SaveSession(w, r, sessData); err != nil {
		l.Error("failed to save session", "data", sessData, "err", err)
		errorCode := "session"
		if errors.Is(err, ErrMaxAccountsReached) {
			errorCode = "max_accounts"
		}
		http.Redirect(w, r, fmt.Sprintf("/login?error=%s", errorCode), http.StatusFound)
		return
	}

	o.Logger.Debug("session saved successfully")

	go o.addToDefaultKnot(sessData.AccountDID)
	go o.addToDefaultSpindle(sessData.AccountDID.String())
	go o.ensureTangledProfile(sessData)
	go o.autoClaimTnglShDomain(sessData.AccountDID.String())

	if !o.Config.Core.Dev {
		err = o.Posthog.Enqueue(posthog.Capture{
			DistinctId: sessData.AccountDID.String(),
			Event:      "signin",
		})
		if err != nil {
			o.Logger.Error("failed to enqueue posthog event", "err", err)
		}
	}

	if redirectURL == "" {
		redirectURL = "/"
	}

	if o.isAccountDeactivated(sessData) {
		redirectURL = "/settings/profile"
	}

	http.Redirect(w, r, redirectURL, http.StatusFound)
}

func (o *OAuth) isAccountDeactivated(sessData *oauth.ClientSessionData) bool {
	pdsClient := &xrpc.Client{
		Host:   sessData.HostURL,
		Client: &http.Client{Timeout: 5 * time.Second},
	}

	_, err := comatproto.RepoDescribeRepo(
		context.Background(),
		pdsClient,
		sessData.AccountDID.String(),
	)
	if err == nil {
		return false
	}

	var xrpcErr *xrpc.Error
	var xrpcBody *xrpc.XRPCError
	return errors.As(err, &xrpcErr) &&
		errors.As(xrpcErr.Wrapped, &xrpcBody) &&
		xrpcBody.ErrStr == "RepoDeactivated"
}

func (o *OAuth) addToDefaultSpindle(did string) {
	l := o.Logger.With("subject", did)

	// use the tangled.sh app password to get an accessJwt
	// and create an sh.tangled.spindle.member record with that
	spindleMembers, err := db.GetSpindleMembers(
		o.Db,
		orm.FilterEq("instance", "spindle.tangled.sh"),
		orm.FilterEq("subject", did),
	)
	if err != nil {
		l.Error("failed to get spindle members", "err", err)
		return
	}

	if len(spindleMembers) != 0 {
		l.Warn("already a member of the default spindle")
		return
	}

	l.Debug("adding to default spindle")
	session, err := o.getAppPasswordSession()
	if err != nil {
		l.Error("failed to create session", "err", err)
		return
	}

	record := tangled.SpindleMember{
		LexiconTypeID: tangled.SpindleMemberNSID,
		Subject:       did,
		Instance:      consts.DefaultSpindle,
		CreatedAt:     time.Now().Format(time.RFC3339),
	}

	if err := session.putRecord(record, tangled.SpindleMemberNSID); err != nil {
		l.Error("failed to add to default spindle", "err", err)
		return
	}

	l.Debug("successfully added to default spindle", "did", did)
}

type onboardAction int

const (
	onboardViaAdminAPI onboardAction = iota
	onboardViaRecord
	onboardBlockedMissingSecret
	onboardBlockedSecretSet
)

type defaultKnotState struct {
	native         bool
	adminSecretSet bool
}

func onboardActionFor(s defaultKnotState) onboardAction {
	switch {
	case s.native && s.adminSecretSet:
		return onboardViaAdminAPI
	case s.native:
		return onboardBlockedMissingSecret
	case s.adminSecretSet:
		return onboardBlockedSecretSet
	default:
		return onboardViaRecord
	}
}

func (o *OAuth) addToDefaultKnot(did syntax.DID) {
	l := o.Logger.With("subject", did)

	ctx := context.Background()

	if o.Acl.IsKnotMember(ctx, o.Config.Knot.Default, did.String()) {
		l.Warn("already a member of the default knot")
		return
	}

	native := knotcompat.KnotHasCapability(ctx, o.Config.Knot.Default, o.Config.Core.Dev, consts.CapKnotACL)

	switch onboardActionFor(defaultKnotState{native: native, adminSecretSet: o.Config.Knot.AdminSecret != ""}) {
	case onboardViaAdminAPI:
		if err := o.addMemberViaKnotAdmin(ctx, o.Config.Knot.Default, did); err != nil {
			l.Error("failed to add to default knot via admin api", "err", err)
			return
		}
		o.Acl.InvalidateMembers(o.Config.Knot.Default)
		l.Debug("successfully added to default knot via admin api")

	case onboardBlockedMissingSecret:
		l.Error("cannot add to default knot: knot admin secret not configured")

	case onboardBlockedSecretSet:
		l.Warn("default knot probe failed, skipping legacy fallback because an admin secret is configured")

	case onboardViaRecord:
		l.Debug("adding to default knot")
		session, err := o.getAppPasswordSession()
		if err != nil {
			l.Error("failed to create session", "err", err)
			return
		}

		record := tangled.KnotMember{
			LexiconTypeID: tangled.KnotMemberNSID,
			Subject:       did.String(),
			Domain:        o.Config.Knot.Default,
			CreatedAt:     time.Now().Format(time.RFC3339),
		}

		if err := session.putRecord(record, tangled.KnotMemberNSID); err != nil {
			l.Error("failed to add to default knot", "err", err)
			return
		}

		if err := o.Enforcer.AddKnotMember(o.Config.Knot.Default, did.String()); err != nil {
			l.Error("failed to set up enforcer rules", "err", err)
			return
		}

		l.Debug("successfully added to default knot")
	}
}

func (o *OAuth) addMemberViaKnotAdmin(ctx context.Context, knotHost string, subject syntax.DID) error {
	ctx, cancel := context.WithTimeout(ctx, knotAdminTimeout)
	defer cancel()

	scheme := "https://"
	if o.Config.Core.Dev {
		scheme = "http://"
	}
	endpoint := fmt.Sprintf("%s%s/admin/addMember", scheme, knotHost)

	body, err := json.Marshal(tangled.KnotAddMember_Input{Subject: subject.String()})
	if err != nil {
		return err
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	req.SetBasicAuth("admin", o.Config.Knot.AdminSecret)

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		msg, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("knot admin addMember returned status %d: %s", resp.StatusCode, bytes.TrimSpace(msg))
	}

	return nil
}

func (o *OAuth) ensureTangledProfile(sessData *oauth.ClientSessionData) {
	ctx := context.Background()
	did := sessData.AccountDID.String()
	l := o.Logger.With("did", did)

	profile, _ := db.GetProfile(o.Db, did)
	if profile != nil {
		l.Debug("profile already exists in DB")
		return
	}

	l.Debug("creating empty Tangled profile")

	sess, err := o.resumeSession(ctx, sessData.AccountDID, sessData.SessionID)
	if err != nil {
		l.Error("failed to resume session for profile creation", "err", err)
		return
	}
	client := sess.APIClient()

	_, err = comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.ActorProfileNSID,
		Repo:       did,
		Rkey:       "self",
		Record:     &lexutil.LexiconTypeDecoder{Val: &tangled.ActorProfile{}},
	})

	if err != nil {
		l.Error("failed to create empty profile on PDS", "err", err)
		return
	}

	emptyProfile := &models.Profile{Did: did}
	if err := db.UpsertProfile(o.Db, emptyProfile); err != nil {
		l.Error("failed to create empty profile in DB", "err", err)
		return
	}

	l.Debug("successfully created empty Tangled profile on PDS and DB")
}

// create a AppPasswordSession using apppasswords
type AppPasswordSession struct {
	AccessJwt   string `json:"accessJwt"`
	RefreshJwt  string `json:"refreshJwt"`
	PdsEndpoint string
	Did         string
	Logger      *slog.Logger
	ExpiresAt   time.Time
}

func CreateAppPasswordSession(res *idresolver.Resolver, appPassword, did string, logger *slog.Logger) (*AppPasswordSession, error) {
	if appPassword == "" {
		return nil, fmt.Errorf("no app password configured")
	}

	resolved, err := res.ResolveIdent(context.Background(), did)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve tangled.sh DID %s: %v", did, err)
	}

	pdsEndpoint := resolved.PDSEndpoint()
	if pdsEndpoint == "" {
		return nil, fmt.Errorf("no PDS endpoint found for tangled.sh DID %s", did)
	}

	sessionPayload := map[string]string{
		"identifier": did,
		"password":   appPassword,
	}
	sessionBytes, err := json.Marshal(sessionPayload)
	if err != nil {
		return nil, fmt.Errorf("failed to marshal session payload: %v", err)
	}

	sessionURL := pdsEndpoint + "/xrpc/com.atproto.server.createSession"
	sessionReq, err := http.NewRequestWithContext(context.Background(), "POST", sessionURL, bytes.NewBuffer(sessionBytes))
	if err != nil {
		return nil, fmt.Errorf("failed to create session request: %v", err)
	}
	sessionReq.Header.Set("Content-Type", "application/json")

	logger.Debug("creating app password session", "url", sessionURL, "headers", sessionReq.Header)

	client := &http.Client{Timeout: 30 * time.Second}
	sessionResp, err := client.Do(sessionReq)
	if err != nil {
		return nil, fmt.Errorf("failed to create session: %v", err)
	}
	defer sessionResp.Body.Close()

	if sessionResp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("failed to create session: HTTP %d", sessionResp.StatusCode)
	}

	var session AppPasswordSession
	if err := json.NewDecoder(sessionResp.Body).Decode(&session); err != nil {
		return nil, fmt.Errorf("failed to decode session response: %v", err)
	}

	session.PdsEndpoint = pdsEndpoint
	session.Did = did
	session.Logger = logger
	session.ExpiresAt = time.Now().Add(115 * time.Minute)

	return &session, nil
}

func (s *AppPasswordSession) RefreshSession() error {
	refreshURL := s.PdsEndpoint + "/xrpc/com.atproto.server.refreshSession"
	req, err := http.NewRequestWithContext(context.Background(), "POST", refreshURL, nil)
	if err != nil {
		return fmt.Errorf("failed to create refresh request: %w", err)
	}

	req.Header.Set("Authorization", "Bearer "+s.RefreshJwt)

	s.Logger.Debug("refreshing app password session", "url", refreshURL)

	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("failed to refresh session: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var errorResponse map[string]any
		if err := json.NewDecoder(resp.Body).Decode(&errorResponse); err != nil {
			return fmt.Errorf("failed to refresh session: HTTP %d (failed to decode error response: %w)", resp.StatusCode, err)
		}
		errorBytes, _ := json.Marshal(errorResponse)
		return fmt.Errorf("failed to refresh session: HTTP %d, response: %s", resp.StatusCode, string(errorBytes))
	}

	var refreshResponse struct {
		AccessJwt  string `json:"accessJwt"`
		RefreshJwt string `json:"refreshJwt"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&refreshResponse); err != nil {
		return fmt.Errorf("failed to decode refresh response: %w", err)
	}

	s.AccessJwt = refreshResponse.AccessJwt
	s.RefreshJwt = refreshResponse.RefreshJwt
	// Set new expiry time with 5 minute buffer
	s.ExpiresAt = time.Now().Add(115 * time.Minute)

	s.Logger.Debug("successfully refreshed app password session")
	return nil
}

func (s *AppPasswordSession) IsValid() bool {
	return time.Now().Before(s.ExpiresAt)
}

func (s *AppPasswordSession) putRecord(record any, collection string) error {
	if !s.IsValid() {
		s.Logger.Debug("access token expired, refreshing session")
		if err := s.RefreshSession(); err != nil {
			return fmt.Errorf("failed to refresh session: %w", err)
		}
		s.Logger.Debug("session refreshed")
	}

	recordBytes, err := json.Marshal(record)
	if err != nil {
		return fmt.Errorf("failed to marshal knot member record: %w", err)
	}

	payload := map[string]any{
		"repo":       s.Did,
		"collection": collection,
		"rkey":       tid.TID(),
		"record":     json.RawMessage(recordBytes),
	}

	payloadBytes, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("failed to marshal request payload: %w", err)
	}

	url := s.PdsEndpoint + "/xrpc/com.atproto.repo.putRecord"
	req, err := http.NewRequestWithContext(context.Background(), "POST", url, bytes.NewBuffer(payloadBytes))
	if err != nil {
		return fmt.Errorf("failed to create HTTP request: %w", err)
	}

	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+s.AccessJwt)

	s.Logger.Debug("putting record", "url", url, "collection", collection)

	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("failed to add user to default service: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var errorResponse map[string]any
		if err := json.NewDecoder(resp.Body).Decode(&errorResponse); err != nil {
			return fmt.Errorf("failed to add user to default service: HTTP %d (failed to decode error response: %w)", resp.StatusCode, err)
		}
		return fmt.Errorf("failed to add user to default service: HTTP %d, response: %v", resp.StatusCode, errorResponse)
	}

	return nil
}

// autoClaimTnglShDomain checks if the user has a .tngl.sh handle and, if so,
// ensures their corresponding sites domain is claimed. This is idempotent —
// ClaimDomain is a no-op if the claim already exists.
func (o *OAuth) autoClaimTnglShDomain(did string) {
	l := o.Logger.With("did", did)

	pdsDomain := strings.TrimPrefix(o.Config.Pds.Host, "https://")
	pdsDomain = strings.TrimPrefix(pdsDomain, "http://")

	resolved, err := o.IdResolver.ResolveIdent(context.Background(), did)
	if err != nil {
		l.Error("autoClaimTnglShDomain: failed to resolve ident", "err", err)
		return
	}

	handle := resolved.Handle.String()
	if !strings.HasSuffix(handle, "."+pdsDomain) {
		return
	}

	if err := db.ClaimDomain(o.Db, did, handle); err != nil {
		l.Warn("autoClaimTnglShDomain: failed to claim domain", "domain", handle, "err", err)
	} else {
		l.Info("autoClaimTnglShDomain: claimed domain", "domain", handle)
	}
}

// getAppPasswordSession returns a cached AppPasswordSession, creating one if needed.
func (o *OAuth) getAppPasswordSession() (*AppPasswordSession, error) {
	o.appPasswordSessionMu.Lock()
	defer o.appPasswordSessionMu.Unlock()

	if o.appPasswordSession != nil {
		return o.appPasswordSession, nil
	}

	session, err := CreateAppPasswordSession(o.IdResolver, o.Config.Core.AppPassword, consts.TangledDid, o.Logger)
	if err != nil {
		return nil, err
	}

	o.appPasswordSession = session
	return session, nil
}
