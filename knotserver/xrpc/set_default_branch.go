package xrpc

import (
	"encoding/json"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventstream"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/tid"

	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) SetDefaultBranch(w http.ResponseWriter, r *http.Request) {
	l := x.Logger

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoSetDefaultBranch_Input
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

	err = gr.SetDefaultBranch(data.DefaultBranch)
	if err != nil {
		l.Error("setting default branch", "error", err.Error())
		serverError(xrpcerr.GitError(err)).send(l, w)
		return
	}

	ownerDid := repo.owner.String()
	refUpdate := tangled.GitRefUpdate{
		Repo:         repo.did.String(),
		OwnerDid:     &ownerDid,
		CommitterDid: actorDid.String(),
	}
	eventJson, err := json.Marshal(refUpdate)
	if err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	if err := x.Db.InsertEvent(eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      tangled.GitRefUpdateNSID,
		EventJson: eventJson,
	}, x.Notifier); err != nil {
		l.Error("failed to insert event", "error", err)
		serverError(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	w.WriteHeader(http.StatusOK)
}
