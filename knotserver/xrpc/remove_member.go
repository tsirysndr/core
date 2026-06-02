package xrpc

import (
	"database/sql"
	"encoding/json"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (h *Xrpc) RemoveMember(w http.ResponseWriter, r *http.Request) {
	l := h.Logger.With("handler", "RemoveMember")
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	allowed, err := h.Enforcer.IsKnotInviteAllowed(actorDid.String(), rbac.ThisServer)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}
	if !allowed {
		fail(xrpcerr.AccessControlError(actorDid.String()), http.StatusForbidden)
		return
	}

	var data tangled.KnotRemoveMember_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	subject, err := syntax.ParseDID(data.Subject)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	isOwner, err := h.Enforcer.IsKnotOwner(subject.String(), rbac.ThisServer)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}
	if isOwner {
		fail(xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("cannot remove the knot owner"),
		), http.StatusBadRequest)
		return
	}

	status, xerr := h.applyAclRevoke(r.Context(), l, aclRevoke{
		role:    "member",
		subject: subject,
		inAcl: func() (bool, error) {
			return h.Enforcer.IsKnotMember(subject.String(), rbac.ThisServer)
		},
		inTable: func() (bool, error) {
			n, err := db.CountKnotMembersBySubject(h.Db, subject.String())
			return n > 0, err
		},
		removeAcl: func() (bool, error) {
			return h.Enforcer.TryRemoveKnotMember(rbac.ThisServer, subject.String())
		},
		restoreAcl: func() error {
			_, err := h.Enforcer.TryAddKnotMember(rbac.ThisServer, subject.String())
			return err
		},
		deleteRow: func(tx *sql.Tx) error {
			return db.RemoveKnotMemberBySubject(tx, subject)
		},
		emit: func() error {
			return h.Db.EmitKnotMemberUpdate(h.Notifier, db.AclOpRemove, subject)
		},
	})
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	w.WriteHeader(status)
}
