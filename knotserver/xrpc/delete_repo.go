package xrpc

import (
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"os"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	securejoin "github.com/cyphar/filepath-securejoin"
	"tangled.org/core/api/tangled"
	"tangled.org/core/rbac"
	"tangled.org/core/repoident"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) DeleteRepo(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "DeleteRepo")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoDelete_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	repoDid, err := repoident.NewRepoDid(data.Repo)
	if err != nil {
		badRequest(xrpcerr.InvalidRepoError(data.Repo)).send(l, w)
		return
	}
	repo := repoDid.String()

	ownerDid, rkey, err := x.Db.GetRepoKeyOwner(repo)
	if errors.Is(err, sql.ErrNoRows) {
		l.Info("repo already torn down or not found", "repo", repo)
		w.WriteHeader(http.StatusOK)
		return
	}
	if err != nil {
		l.Error("failed to look up repo", "error", err.Error())
		serverError(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	ident, err := x.Resolver.ResolveIdent(r.Context(), ownerDid)
	if err != nil || ident.Handle.IsInvalidHandle() {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	xrpcc := xrpc.Client{
		Host: ident.PDSEndpoint(),
	}

	// ensure that the record does not exists
	_, err = comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoNSID, ownerDid, rkey)
	if err == nil {
		badRequest(xrpcerr.RecordExistsError(rkey)).send(l, w)
		return
	}

	repoPath, joinErr := securejoin.SecureJoin(x.Config.Repo.ScanPath, repo)
	if joinErr != nil {
		badRequest(xrpcerr.GenericError(joinErr)).send(l, w)
		return
	}

	isDeleteAllowed, err := x.Enforcer.IsRepoDeleteAllowed(actorDid.String(), rbac.ThisServer, repo)
	if err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}
	if !isDeleteAllowed {
		badRequest(xrpcerr.AccessControlError(actorDid.String())).send(l, w)
		return
	}

	if rmErr := os.RemoveAll(repoPath); rmErr != nil {
		l.Error("deleting repo", "error", rmErr.Error())
		serverError(xrpcerr.GenericError(rmErr)).send(l, w)
		return
	}

	if rbacErr := x.Enforcer.WipeRepoPolicies(rbac.ThisServer, repo); rbacErr != nil {
		l.Error("failed to delete repo from enforcer", "error", rbacErr.Error())
		serverError(xrpcerr.GenericError(rbacErr)).send(l, w)
		return
	}

	if delErr := x.Db.DeleteRepoKey(repo); delErr != nil {
		l.Error("failed to delete repo key", "error", delErr.Error())
		serverError(xrpcerr.GenericError(delErr)).send(l, w)
		return
	}

	w.WriteHeader(http.StatusOK)
}
