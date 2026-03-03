package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) DeleteRepo(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "DeleteRepo")
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	var data tangled.RepoDelete_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	did := data.Did
	name := data.Name
	rkey := data.Rkey

	if did == "" || name == "" {
		fail(xrpcerr.GenericError(fmt.Errorf("did and name are required")))
		return
	}

	ident, err := x.Resolver.ResolveIdent(r.Context(), actorDid.String())
	if err != nil || ident.Handle.IsInvalidHandle() {
		fail(xrpcerr.GenericError(err))
		return
	}

	xrpcc := xrpc.Client{
		Host: ident.PDSEndpoint(),
	}

	// ensure that the record does not exists
	_, err = comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoNSID, actorDid.String(), rkey)
	if err == nil {
		fail(xrpcerr.RecordExistsError(rkey))
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

	isDeleteAllowed, err := x.Enforcer.IsRepoDeleteAllowed(actorDid.String(), rbac.ThisServer, repoDid)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}
	if !isDeleteAllowed {
		fail(xrpcerr.AccessControlError(actorDid.String()))
		return
	}

	err = os.RemoveAll(repoPath)
	if err != nil {
		l.Error("deleting repo", "error", err.Error())
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	err = x.Enforcer.RemoveRepo(did, rbac.ThisServer, repoDid)
	if err != nil {
		l.Error("failed to delete repo from enforcer", "error", err.Error())
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	if err := x.Db.DeleteRepoKey(repoDid); err != nil {
		l.Error("failed to delete repo key", "error", err.Error())
	}

	w.WriteHeader(http.StatusOK)
}
