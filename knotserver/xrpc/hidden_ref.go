package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) HiddenRef(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "HiddenRef")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoHiddenRef_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	forkRef := data.ForkRef
	remoteRef := data.RemoteRef

	if forkRef == "" || remoteRef == "" {
		badRequest(xrpcerr.GenericError(fmt.Errorf("forkRef and remoteRef are required"))).send(l, w)
		return
	}

	repo, denial := x.pushableRepoDID(actorDid, data.Repo)
	if denial != nil {
		denial.send(l, w)
		return
	}

	gr, err := git.PlainOpen(repo.path)
	if err != nil {
		badRequest(xrpcerr.GenericError(fmt.Errorf("failed to open repository: %w", err))).send(l, w)
		return
	}

	err = gr.TrackHiddenRemoteRef(forkRef, remoteRef)
	if err != nil {
		l.Error("error tracking hidden remote ref", "error", err.Error())
		serverError(xrpcerr.GitError(err)).send(l, w)
		return
	}

	response := tangled.RepoHiddenRef_Output{
		Success: true,
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(response)
}
