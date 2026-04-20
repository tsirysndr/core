package xrpc

import (
	"database/sql"
	"errors"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) RepoDescribeRepo(w http.ResponseWriter, r *http.Request) {
	raw := r.URL.Query().Get("repoDid")
	repoDid, err := syntax.ParseDID(raw)
	if err != nil {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("missing or invalid repoDid parameter"),
		), http.StatusBadRequest)
		return
	}

	ownerDid, rkey, err := x.Db.GetRepoKeyOwner(repoDid.String())
	if errors.Is(err, sql.ErrNoRows) {
		writeError(w, xrpcerr.RepoNotFoundError, http.StatusNotFound)
		return
	}
	if err != nil {
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	x.writeJson(w, tangled.RepoDescribeRepo_Output{
		RepoDid:  repoDid.String(),
		OwnerDid: ownerDid,
		Rkey:     rkey,
	})
}
