package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventstream"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/rbac"
	"tangled.org/core/tid"

	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) SetDefaultBranch(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	var data tangled.RepoSetDefaultBranch_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	// unfortunately we have to resolve repo-at here
	repoAt, err := syntax.ParseATURI(data.Repo)
	if err != nil {
		fail(xrpcerr.InvalidRepoError(data.Repo))
		return
	}

	// resolve this aturi to extract the repo record
	ident, err := x.Resolver.ResolveIdent(r.Context(), repoAt.Authority().String())
	if err != nil || ident.Handle.IsInvalidHandle() {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to resolve handle: %w", err)))
		return
	}

	xrpcc := xrpc.Client{Host: ident.PDSEndpoint()}
	resp, err := comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	if _, ok := resp.Value.Val.(*tangled.Repo); !ok {
		fail(xrpcerr.RepoNotFoundError)
		return
	}
	repoDid, err := x.Db.GetRepoDid(actorDid.String(), repoAt.RecordKey().String())
	if err != nil {
		fail(xrpcerr.RepoNotFoundError)
		return
	}
	repoPath, _, _, err := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repoDid)
	if err != nil {
		fail(xrpcerr.RepoNotFoundError)
		return
	}

	if ok, err := x.Enforcer.IsPushAllowed(actorDid.String(), rbac.ThisServer, repoDid); !ok || err != nil {
		l.Error("insufficient permissions", "did", actorDid.String())
		writeError(w, xrpcerr.AccessControlError(actorDid.String()), http.StatusUnauthorized)
		return
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	err = gr.SetDefaultBranch(data.DefaultBranch)
	if err != nil {
		l.Error("setting default branch", "error", err.Error())
		writeError(w, xrpcerr.GitError(err), http.StatusInternalServerError)
		return
	}

	ownerDid := ident.DID.String()
	refUpdate := tangled.GitRefUpdate{
		Repo:         repoDid,
		OwnerDid:     &ownerDid,
		CommitterDid: actorDid.String(),
	}
	eventJson, err := json.Marshal(refUpdate)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	if err := x.Db.InsertEvent(eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      tangled.GitRefUpdateNSID,
		EventJson: eventJson,
	}, x.Notifier); err != nil {
		l.Error("failed to insert event", "error", err)
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}
