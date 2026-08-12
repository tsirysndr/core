package xrpc

import (
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"strings"

	"tangled.org/core/api/tangled"
	db "tangled.org/core/deliberi/db"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) AccountListEmails(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AccountListEmails")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	emails, err := db.GetAllEmails(x.DB, did)
	if err != nil {
		l.Error("failed to get emails", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	items := make([]*tangled.TempAccountListEmails_Email, 0, len(emails))
	for _, e := range emails {
		items = append(items, &tangled.TempAccountListEmails_Email{
			Address:   e.Address,
			Verified:  e.Verified,
			Primary:   e.Primary,
			CreatedAt: e.CreatedAt.UTC().Format(timeFormat),
		})
	}

	x.writeJSON(w, &tangled.TempAccountListEmails_Output{Emails: items})
}

func (x *Xrpc) AccountDeleteEmail(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AccountDeleteEmail")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempAccountDeleteEmail_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}
	addr := strings.TrimSpace(input.Email)

	existing, err := db.GetEmail(x.DB, did, addr)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			writeError(w, xrpcErrorTag("EmailNotFound", "the email address is not associated with this account"), http.StatusNotFound)
			return
		}
		l.Error("failed to get email", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if existing.Primary {
		writeError(w, xrpcErrorTag("CannotDeletePrimary", "the primary email address cannot be deleted; set another address as primary first"), http.StatusBadRequest)
		return
	}

	if err := db.DeleteEmail(x.DB, did, addr); err != nil {
		l.Error("failed to delete email", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) AccountSetPrimaryEmail(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AccountSetPrimaryEmail")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempAccountSetPrimaryEmail_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}
	addr := strings.TrimSpace(input.Email)

	existing, err := db.GetEmail(x.DB, did, addr)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			writeError(w, xrpcErrorTag("EmailNotFound", "the email address is not associated with this account"), http.StatusNotFound)
			return
		}
		l.Error("failed to get email", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if !existing.Verified {
		writeError(w, xrpcErrorTag("EmailNotVerified", "the email address must be verified before it can be made primary"), http.StatusBadRequest)
		return
	}

	if err := db.MakeEmailPrimary(x.DB, did, addr); err != nil {
		l.Error("failed to set primary email", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}
