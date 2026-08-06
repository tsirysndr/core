package xrpc

import (
	"encoding/json"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"

	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) DeleteBranch(w http.ResponseWriter, r *http.Request) {
	l := x.Logger

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoDeleteBranch_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	repo, denial := x.pushableRepoDID(actorDid, data.Repo)
	if denial != nil {
		denial.send(l, w)
		return
	}

	gr, err := git.PlainOpen(repo.path)
	if err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	err = gr.DeleteBranch(data.Branch)
	if err != nil {
		l.Error("deleting branch", "error", err.Error(), "branch", data.Branch)
		serverError(xrpcerr.GitError(err)).send(l, w)
		return
	}

	w.WriteHeader(http.StatusOK)
}
