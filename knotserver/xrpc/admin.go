package xrpc

import (
	"crypto/subtle"
	"encoding/json"
	"errors"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	xrpcerr "tangled.org/core/xrpc/errors"
)

const maxAdminBodyBytes = 4 << 10

func (x *Xrpc) AdminRouter() http.Handler {
	r := chi.NewRouter()
	r.Use(x.VerifyAdminSecret)
	r.Post("/addMember", x.AddMemberAdmin)
	return r
}

func (x *Xrpc) VerifyAdminSecret(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		secret := x.Config.Server.AdminSecret
		user, pass, ok := r.BasicAuth()
		valid := secret != "" &&
			ok &&
			user == "admin" &&
			subtle.ConstantTimeCompare([]byte(pass), []byte(secret)) == 1
		if !valid {
			writeError(w, xrpcerr.AuthError(errors.New("invalid admin credentials")), http.StatusUnauthorized)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func (x *Xrpc) AddMemberAdmin(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AddMemberAdmin")
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	var data tangled.KnotAddMember_Input
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxAdminBodyBytes)).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	subject, err := syntax.ParseDID(data.Subject)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	owner, err := syntax.ParseDID(x.Config.Server.Owner)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	status, xerr := x.addMemberToKnot(r.Context(), l, owner, subject)
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	w.WriteHeader(status)
}
