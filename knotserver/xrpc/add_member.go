package xrpc

import (
	"context"
	"database/sql"
	"encoding/json"
	"log/slog"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

const keyFetchTimeout = 15 * time.Second

func (h *Xrpc) AddMember(w http.ResponseWriter, r *http.Request) {
	l := h.Logger.With("handler", "AddMember")
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

	var data tangled.KnotAddMember_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	subject, err := syntax.ParseDID(data.Subject)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	status, xerr := h.addMemberToKnot(r.Context(), l, actorDid, subject)
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	w.WriteHeader(status)
}

func (h *Xrpc) addMemberToKnot(ctx context.Context, l *slog.Logger, addedBy, subject syntax.DID) (int, *xrpcerr.XrpcError) {
	isOwner, err := h.Enforcer.IsKnotOwner(subject.String(), rbac.ThisServer)
	if err != nil {
		e := xrpcerr.GenericError(err)
		return http.StatusInternalServerError, &e
	}
	if isOwner {
		l.Info("subject is the knot owner, no-op", "subject", subject)
		return http.StatusOK, nil
	}

	return h.applyAclGrant(ctx, l, aclGrant{
		role:    "member",
		subject: subject,
		inAcl: func() (bool, error) {
			return h.Enforcer.IsKnotMember(subject.String(), rbac.ThisServer)
		},
		inTable: func() (bool, error) {
			n, err := db.CountKnotMembersBySubject(h.Db, subject.String())
			return n > 0, err
		},
		insertRow: func(tx *sql.Tx) error {
			return db.AddKnotMemberDirect(tx, addedBy, subject)
		},
		deleteRow: func() error {
			return db.RemoveKnotMemberDirect(h.Db, subject)
		},
		grantAcl: func() error {
			_, err := h.Enforcer.TryAddKnotMember(rbac.ThisServer, subject.String())
			return err
		},
		emit: func() error {
			return h.Db.EmitKnotMemberUpdate(h.Notifier, db.AclOpAdd, subject)
		},
	})
}
