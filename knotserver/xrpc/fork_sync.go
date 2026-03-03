package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) ForkSync(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "ForkSync")
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	var data tangled.RepoForkSync_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	did := data.Did
	name := data.Name
	branch := data.Branch

	if did == "" || name == "" {
		fail(xrpcerr.GenericError(fmt.Errorf("did, name are required")))
		return
	}

	repoDid, err := x.Db.GetRepoDid(did, name)
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
		l.Error("insufficient permissions", "did", actorDid.String(), "repo", repoDid)
		writeError(w, xrpcerr.AccessControlError(actorDid.String()), http.StatusUnauthorized)
		return
	}

	gr, err := git.Open(repoPath, branch)
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to open repository: %w", err)))
		return
	}

	err = gr.Sync()
	if err != nil {
		l.Error("error syncing repo fork", "error", err.Error())
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}
