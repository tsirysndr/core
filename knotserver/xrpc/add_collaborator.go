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

type collabTarget struct {
	repoDid   syntax.DID
	subject   syntax.DID
	ownerNoop bool
}

func (h *Xrpc) resolveCollabTarget(actor syntax.DID, repo, subject string) (collabTarget, int, *xrpcerr.XrpcError) {
	fail := func(status int, e xrpcerr.XrpcError) (collabTarget, int, *xrpcerr.XrpcError) {
		return collabTarget{}, status, &e
	}

	repoDid, err := syntax.ParseDID(repo)
	if err != nil {
		return fail(http.StatusBadRequest, xrpcerr.InvalidRepoError(repo))
	}
	subjectDid, err := syntax.ParseDID(subject)
	if err != nil {
		return fail(http.StatusBadRequest, xrpcerr.GenericError(err))
	}

	exists, err := h.Db.RepoDidExists(repoDid.String())
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if !exists {
		return fail(http.StatusNotFound, xrpcerr.RepoNotFoundError)
	}

	allowed, err := h.Enforcer.IsCollaboratorInviteAllowed(actor.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if !allowed {
		return fail(http.StatusForbidden, xrpcerr.AccessControlError(actor.String()))
	}

	isOwner, err := h.Enforcer.IsRepoOwner(subjectDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}

	return collabTarget{repoDid: repoDid, subject: subjectDid, ownerNoop: isOwner}, 0, nil
}

func (h *Xrpc) AddCollaborator(w http.ResponseWriter, r *http.Request) {
	l := h.Logger.With("handler", "AddCollaborator")
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var data tangled.RepoAddCollaborator_Input
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

	status, xerr = h.applyAclGrant(r.Context(), l.With("repoDid", t.repoDid), aclGrant{
		role:    "collaborator",
		subject: t.subject,
		inAcl: func() (bool, error) {
			return h.Enforcer.IsRepoCollaborator(t.subject.String(), rbac.ThisServer, t.repoDid.String())
		},
		inTable: func() (bool, error) {
			return db.IsCollaborator(h.Db, t.repoDid, t.subject)
		},
		insertRow: func(tx *sql.Tx) error {
			return db.AddCollaborator(tx, db.Collaborator{
				RepoDid: t.repoDid,
				Subject: t.subject,
				AddedBy: actorDid,
			})
		},
		deleteRow: func() error {
			return db.RemoveCollaborator(h.Db, t.repoDid, t.subject)
		},
		grantAcl: func() error {
			return h.Enforcer.AddCollaborator(t.subject.String(), rbac.ThisServer, t.repoDid.String())
		},
		emit: func() error {
			return h.Db.EmitCollaboratorUpdate(h.Notifier, db.AclOpAdd, t.subject, t.repoDid)
		},
	})
	if xerr != nil {
		fail(*xerr, status)
		return
	}
	w.WriteHeader(status)
}
