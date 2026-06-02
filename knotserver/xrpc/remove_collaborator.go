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

func (h *Xrpc) RemoveCollaborator(w http.ResponseWriter, r *http.Request) {
	l := h.Logger.With("handler", "RemoveCollaborator")
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var data tangled.RepoRemoveCollaborator_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusBadRequest)
		return
	}

	t, status, xerr := h.resolveCollabTarget(actorDid, data.Repo, data.Subject)
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	if t.ownerNoop {
		l.Info("subject is the repo owner, no-op", "repoDid", t.repoDid, "subject", t.subject)
		w.WriteHeader(http.StatusOK)
		return
	}

	status, xerr = h.applyAclRevoke(r.Context(), l.With("repoDid", t.repoDid), aclRevoke{
		role:    "collaborator",
		subject: t.subject,
		inAcl: func() (bool, error) {
			return h.Enforcer.IsRepoCollaborator(t.subject.String(), rbac.ThisServer, t.repoDid.String())
		},
		inTable: func() (bool, error) {
			return db.IsCollaborator(h.Db, t.repoDid, t.subject)
		},
		removeAcl: func() (bool, error) {
			if err := h.Enforcer.RemoveCollaborator(t.subject.String(), rbac.ThisServer, t.repoDid.String()); err != nil {
				return false, err
			}
			return true, nil
		},
		restoreAcl: func() error {
			return h.Enforcer.AddCollaborator(t.subject.String(), rbac.ThisServer, t.repoDid.String())
		},
		deleteRow: func(tx *sql.Tx) error {
			return db.RemoveCollaborator(tx, t.repoDid, t.subject)
		},
		emit: func() error {
			return h.Db.EmitCollaboratorUpdate(h.Notifier, db.AclOpRemove, t.subject, t.repoDid)
		},
	})
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	w.WriteHeader(status)
}
