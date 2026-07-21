package xrpc

import (
	"encoding/json"
	"log/slog"
	"net/http"
	"runtime/debug"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/codesearch"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	whnotify "tangled.org/core/appview/notify/webhook"
	"tangled.org/core/idresolver"
	xrpcerr "tangled.org/core/xrpc/errors"
	"tangled.org/core/xrpc/serviceauth"
)

const ActorDid = serviceauth.ActorDid

type Xrpc struct {
	DB          *db.DB
	Config      *config.Config
	Logger      *slog.Logger
	ServiceAuth *serviceauth.ServiceAuth
	IdResolver  *idresolver.Resolver
	Cloudflare  *cloudflare.Client
	CodeSearch  *codesearch.CodeSearch
	Webhooks    *whnotify.Notifier

	// reserved usernames rejected at signup completion
	DisallowedNicknames map[string]bool
}

func (x *Xrpc) Router() http.Handler {
	r := chi.NewRouter()

	r.Use(x.cors)

	// health check, atproto _health convention
	r.Get("/_health", x.health)

	// open endpoints: signup happens pre-identity, so no service auth
	r.Post("/"+tangled.TempAccountBeginSignupNSID, x.AccountBeginSignup)
	r.Post("/"+tangled.TempAccountCompleteSignupNSID, x.AccountCompleteSignup)

	// authenticated endpoints
	r.Group(func(r chi.Router) {
		r.Use(x.ServiceAuth.VerifyServiceAuth)

		// code search is gated on login, matching the appview ui
		r.Get("/"+tangled.TempSearchSearchCodeNSID, x.SearchSearchCode)

		// notifications
		r.Get("/"+tangled.TempNotificationListNotificationsNSID, x.NotificationList)
		r.Get("/"+tangled.TempNotificationGetUnreadCountNSID, x.NotificationGetUnreadCount)
		r.Post("/"+tangled.TempNotificationUpdateSeenNSID, x.NotificationUpdateSeen)
		r.Post("/"+tangled.TempNotificationMarkAllReadNSID, x.NotificationMarkAllRead)
		r.Post("/"+tangled.TempNotificationDeleteNotificationNSID, x.NotificationDelete)
		r.Get("/"+tangled.TempNotificationGetPreferencesNSID, x.NotificationGetPreferences)
		r.Post("/"+tangled.TempNotificationUpdatePreferencesNSID, x.NotificationUpdatePreferences)

		// focus mode
		r.Post("/"+tangled.TempFocusBeginSessionNSID, x.FocusBegin)
		r.Post("/"+tangled.TempFocusNextItemNSID, x.FocusNext)
		r.Post("/"+tangled.TempFocusEndSessionNSID, x.FocusEnd)

		// account management
		r.Get("/"+tangled.TempAccountListEmailsNSID, x.AccountListEmails)
		r.Post("/"+tangled.TempAccountDeleteEmailNSID, x.AccountDeleteEmail)
		r.Post("/"+tangled.TempAccountSetPrimaryEmailNSID, x.AccountSetPrimaryEmail)
		r.Post("/"+tangled.TempAccountSubscribeNewsletterNSID, x.AccountSubscribeNewsletter)
		r.Post("/"+tangled.TempAccountDismissNewsletterNSID, x.AccountDismissNewsletter)

		// webhooks
		r.Get("/"+tangled.TempRepoListWebhooksNSID, x.WebhookList)
		r.Post("/"+tangled.TempRepoCreateWebhookNSID, x.WebhookCreate)
		r.Post("/"+tangled.TempRepoUpdateWebhookNSID, x.WebhookUpdate)
		r.Post("/"+tangled.TempRepoDeleteWebhookNSID, x.WebhookDelete)
		r.Post("/"+tangled.TempRepoToggleWebhookNSID, x.WebhookToggle)
		r.Get("/"+tangled.TempRepoListWebhookDeliveriesNSID, x.WebhookListDeliveries)
		r.Post("/"+tangled.TempRepoRetryWebhookDeliveryNSID, x.WebhookRetryDelivery)

		// sites
		r.Get("/"+tangled.TempSiteGetDomainClaimNSID, x.SiteGetDomainClaim)
		r.Post("/"+tangled.TempSiteClaimDomainNSID, x.SiteClaimDomain)
		r.Post("/"+tangled.TempSiteReleaseDomainNSID, x.SiteReleaseDomain)
		r.Get("/"+tangled.TempRepoGetSiteConfigNSID, x.SiteGetRepoSiteConfig)
		r.Post("/"+tangled.TempRepoUpdateSiteConfigNSID, x.SiteUpdateRepoSiteConfig)
		r.Post("/"+tangled.TempRepoDisableSiteNSID, x.SiteDisableRepoSite)
	})

	return r
}

// timeFormat is the datetime format used across lexicon output fields
const timeFormat = "2006-01-02T15:04:05.000Z"

// health responds to /xrpc/_health with the running version
func (x *Xrpc) health(w http.ResponseWriter, r *http.Request) {
	x.writeJSON(w, map[string]string{"version": serviceVersion()})
}

// serviceVersion returns the build's vcs revision, or "dev"
func serviceVersion() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "dev"
	}
	for _, s := range info.Settings {
		if s.Key == "vcs.revision" && s.Value != "" {
			return s.Value
		}
	}
	return "dev"
}

// cors allows the browser origin to call the xrpc endpoints. auth is via
// bearer tokens, not cookies, so a wildcard origin is safe.
func (x *Xrpc) cors(next http.Handler) http.Handler {
	origin := x.Config.Core.XrpcCorsOrigin
	if origin == "" {
		origin = "*"
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", origin)
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		w.Header().Set("Access-Control-Max-Age", "86400")
		if origin != "*" {
			w.Header().Add("Vary", "Origin")
		}
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func writeError(w http.ResponseWriter, e xrpcerr.XrpcError, status int) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(e)
}

func (x *Xrpc) writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(v)
}

func actorDid(r *http.Request) (string, bool) {
	did, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		return "", false
	}
	return did.String(), true
}

// stable client-facing errors; handlers log the real cause and return these
var (
	errInternal       = xrpcErrorTag("InternalError", "internal server error")
	errBadRequestBody = xrpcErrorTag("InvalidRequest", "invalid request body")
	errUpstream       = xrpcErrorTag("UpstreamError", "an upstream service failed")
)

func xrpcErrorTag(tag, message string) xrpcerr.XrpcError {
	return xrpcerr.NewXrpcError(xrpcerr.WithTag(tag), xrpcerr.WithMessage(message))
}

func badRequestError(message string) xrpcerr.XrpcError {
	return xrpcErrorTag("InvalidRequest", message)
}

func notFoundError(message string) xrpcerr.XrpcError {
	return xrpcErrorTag("NotFound", message)
}

func notImplementedError(message string) xrpcerr.XrpcError {
	return xrpcErrorTag("MethodNotImplemented", message)
}
