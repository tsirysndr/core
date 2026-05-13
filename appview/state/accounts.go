package state

import (
	"net/http"

	"github.com/go-chi/chi/v5"
)

func (s *State) SwitchAccount(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "SwitchAccount")

	if err := r.ParseForm(); err != nil {
		l.Error("failed to parse form", "err", err)
		http.Error(w, "invalid request", http.StatusBadRequest)
		return
	}

	did := r.FormValue("did")
	if did == "" {
		http.Error(w, "missing did", http.StatusBadRequest)
		return
	}

	if err := s.oauth.SwitchAccount(w, r, did); err != nil {
		l.Error("failed to switch account", "err", err)
		redirectURL, err := s.oauth.ClientApp.StartAuthFlow(r.Context(), did)
		if err != nil {
			l.Error("failed to resume login flow", "err", err)
			s.pages.HxRedirect(w, "/login?error=session")
			return
		}
		s.pages.HxRedirect(w, redirectURL)
		return
	}

	l.Info("switched account", "did", did)
	if returnUrl := r.FormValue("return_url"); returnUrl != "" {
		s.pages.HxRedirect(w, returnUrl)
	} else {
		s.pages.HxRefresh(w)
	}
}

func (s *State) RemoveAccount(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "RemoveAccount")

	did := chi.URLParam(r, "did")
	if did == "" {
		http.Error(w, "missing did", http.StatusBadRequest)
		return
	}

	currentUser := s.oauth.GetMultiAccountUser(r)
	isCurrentAccount := currentUser != nil && currentUser.Did == did

	var remainingAccounts []string
	if currentUser != nil {
		for _, acc := range currentUser.Accounts {
			if acc.Did != did {
				remainingAccounts = append(remainingAccounts, acc.Did)
			}
		}
	}

	if err := s.oauth.RemoveAccount(w, r, did); err != nil {
		l.Error("failed to remove account", "err", err)
		http.Error(w, "failed to remove account", http.StatusInternalServerError)
		return
	}

	l.Info("removed account", "did", did)

	if isCurrentAccount {
		if len(remainingAccounts) > 0 {
			nextDid := remainingAccounts[0]
			if err := s.oauth.SwitchAccount(w, r, nextDid); err != nil {
				l.Error("failed to switch to next account", "err", err)
				s.pages.HxRedirect(w, "/login")
				return
			}
			s.pages.HxRefresh(w)
			return
		}

		if err := s.oauth.DeleteSession(w, r); err != nil {
			l.Error("failed to delete session", "err", err)
		}
		s.pages.HxRedirect(w, "/login")
		return
	}

	s.pages.HxRefresh(w)
}
