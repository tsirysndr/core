package xrpc

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	securejoin "github.com/cyphar/filepath-securejoin"
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
	if errors.Is(err, sql.ErrNoRows) {
		repoDid, err = x.Db.GetRepoDidByName(did, name)
		if errors.Is(err, sql.ErrNoRows) {
			l.Info("repo already torn down or not found", "did", did, "name", name)
			w.WriteHeader(http.StatusOK)
			return
		}
	}
	if err != nil {
		l.Error("failed to look up repo", "error", err.Error())
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	repoPath, joinErr := securejoin.SecureJoin(x.Config.Repo.ScanPath, repoDid)
	if joinErr != nil {
		fail(xrpcerr.GenericError(joinErr))
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

	if rmErr := os.RemoveAll(repoPath); rmErr != nil {
		l.Error("deleting repo", "error", rmErr.Error())
		writeError(w, xrpcerr.GenericError(rmErr), http.StatusInternalServerError)
		return
	}

	if rbacErr := x.Enforcer.WipeRepoPolicies(rbac.ThisServer, repoDid); rbacErr != nil {
		l.Error("failed to delete repo from enforcer", "error", rbacErr.Error())
		writeError(w, xrpcerr.GenericError(rbacErr), http.StatusInternalServerError)
		return
	}

	if delErr := x.Db.DeleteRepoKey(repoDid); delErr != nil {
		l.Error("failed to delete repo key", "error", delErr.Error())
		writeError(w, xrpcerr.GenericError(delErr), http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}
