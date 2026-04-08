package state

import (
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
)

func (s *State) Login(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "Login")

	switch r.Method {
	case http.MethodGet:
		returnURL := r.URL.Query().Get("return_url")
		errorCode := r.URL.Query().Get("error")
		addAccount := r.URL.Query().Get("mode") == "add_account"

		user := s.oauth.GetMultiAccountUser(r)
		if user == nil {
			registry := s.oauth.GetAccounts(r)
			if len(registry.Accounts) > 0 {
				user = &oauth.MultiAccountUser{
					Active:   nil,
					Accounts: registry.Accounts,
				}
			}
		}
		s.pages.Login(w, pages.LoginParams{
			ReturnUrl:    returnURL,
			ErrorCode:    errorCode,
			AddAccount:   addAccount,
			LoggedInUser: user,
		})
	case http.MethodPost:
		handle := r.FormValue("handle")
		returnURL := r.FormValue("return_url")
		addAccount := r.FormValue("add_account") == "true"

		// remove spaces around the handle, handles can't have spaces around them
		handle = strings.TrimSpace(handle)

		// when users copy their handle from bsky.app, it tends to have these characters around it:
		//
		// @nelind.dk:
		//   \u202a ensures that the handle is always rendered left to right and
		//   \u202c reverts that so the rest of the page renders however it should
		handle = strings.TrimPrefix(handle, "\u202a")
		handle = strings.TrimSuffix(handle, "\u202c")

		// `@` is harmless
		handle = strings.TrimPrefix(handle, "@")

		// basic handle validation
		if !strings.Contains(handle, ".") {
			l.Error("invalid handle format", "raw", handle)
			s.pages.Notice(
				w,
				"login-msg",
				fmt.Sprintf("\"%s\" is an invalid handle. Did you mean %s.bsky.social or %s.tngl.sh?", handle, handle, handle),
			)
			return
		}

		ident, err := s.idResolver.ResolveIdent(r.Context(), handle)
		if err != nil && errors.Is(err, identity.ErrHandleMismatch) {
			if h, parseErr := syntax.ParseHandle(handle); parseErr == nil {
				if did, resolveErr := s.idResolver.ResolveHandle(r.Context(), h); resolveErr == nil {
					ident, err = s.idResolver.ResolveIdent(r.Context(), did.String())
				}
			}
		}
		if err != nil {
			l.Warn("handle resolution failed", "handle", handle, "err", err)
			s.pages.Notice(w, "login-msg", fmt.Sprintf("Could not resolve handle \"%s\". The account may not exist.", handle))
			return
		}

		pdsEndpoint := ident.PDSEndpoint()
		if pdsEndpoint == "" {
			s.pages.Notice(w, "login-msg", fmt.Sprintf("No PDS found for \"%s\".", handle))
			return
		}

		pdsClient := &xrpc.Client{Host: pdsEndpoint, Client: &http.Client{Timeout: 5 * time.Second}}
		_, err = comatproto.RepoDescribeRepo(r.Context(), pdsClient, ident.DID.String())
		if err != nil {
			var xrpcErr *xrpc.Error
			var xrpcBody *xrpc.XRPCError
			isDeactivated := errors.As(err, &xrpcErr) &&
				errors.As(xrpcErr.Wrapped, &xrpcBody) &&
				xrpcBody.ErrStr == "RepoDeactivated"

			if !isDeactivated {
				l.Warn("describeRepo failed", "handle", handle, "did", ident.DID, "pds", pdsEndpoint, "err", err)
				s.pages.Notice(w, "login-msg", fmt.Sprintf("Account \"%s\" is no longer available.", handle))
				return
			}
		}

		if err := s.oauth.SetAuthReturn(w, r, sanitizeReturnURL(returnURL), addAccount); err != nil {
			l.Error("failed to set auth return", "err", err)
		}

		redirectURL, err := s.oauth.ClientApp.StartAuthFlow(r.Context(), ident.DID.String())
		if err != nil {
			l.Error("failed to start auth", "err", err)
			s.pages.Notice(
				w,
				"login-msg",
				fmt.Sprintf("Failed to start auth flow: %v", err),
			)
			return
		}

		s.pages.HxRedirect(w, redirectURL)
	}
}

// sanitizeReturnURL ensures the return URL is a relative path on the same
// origin. Anything else — absolute URLs, protocol-relative URLs — is replaced
// with "/" to prevent open redirect after OAuth login.
func sanitizeReturnURL(s string) string {
	if strings.HasPrefix(s, "/") && !strings.HasPrefix(s, "//") {
		return s
	}
	return "/"
}

func (s *State) Logout(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "Logout")

	currentUser := s.oauth.GetMultiAccountUser(r)
	if currentUser == nil || currentUser.Active == nil {
		s.pages.HxRedirect(w, "/login")
		return
	}

	currentDid := currentUser.Active.Did

	var remainingAccounts []string
	for _, acc := range currentUser.Accounts {
		if acc.Did != currentDid {
			remainingAccounts = append(remainingAccounts, acc.Did)
		}
	}

	if err := s.oauth.RemoveAccount(w, r, currentDid); err != nil {
		l.Error("failed to remove account from registry", "err", err)
	}

	if err := s.oauth.DeleteSession(w, r); err != nil {
		l.Error("failed to delete session", "err", err)
	}

	if len(remainingAccounts) > 0 {
		nextDid := remainingAccounts[0]
		if err := s.oauth.SwitchAccount(w, r, nextDid); err != nil {
			l.Error("failed to switch to next account", "err", err)
			s.pages.HxRedirect(w, "/login")
			return
		}
		l.Info("switched to next account after logout", "did", nextDid)
		s.pages.HxRefresh(w)
		return
	}

	l.Info("logged out last account")
	s.pages.HxRedirect(w, "/login")
}
