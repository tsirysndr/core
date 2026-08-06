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

func (x *Xrpc) ForkSync(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "ForkSync")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoForkSync_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	repo, denial := x.pushableRepoDID(actorDid, data.Repo)
	if denial != nil {
		denial.send(l, w)
		return
	}

	gr, err := git.Open(repo.path, data.Branch)
	if err != nil {
		badRequest(xrpcerr.GenericError(fmt.Errorf("failed to open repository: %w", err))).send(l, w)
		return
	}

	err = gr.Sync()
	if err != nil {
		l.Error("error syncing repo fork", "error", err.Error())
		serverError(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	w.WriteHeader(http.StatusOK)
}
