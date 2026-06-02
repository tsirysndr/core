package xrpc

import (
	"net/http"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) ListCollaborators(w http.ResponseWriter, r *http.Request) {
	subject := r.URL.Query().Get("subject")
	repoDid, err := syntax.ParseDID(subject)
	if err != nil {
		writeError(w, xrpcerr.InvalidRepoError(subject), http.StatusBadRequest)
		return
	}

	p, err := parseListParams(r)
	if err != nil {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage(err.Error()),
		), http.StatusBadRequest)
		return
	}

	collaborators, next, err := db.ListCollaborators(x.Db, repoDid, p)
	if err != nil {
		x.Logger.Error("failed to list collaborators", "repoDid", repoDid, "error", err)
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InternalServerError"),
			xrpcerr.WithMessage("failed to list collaborators"),
		), http.StatusInternalServerError)
		return
	}

	response := tangled.RepoListCollaborators_Output{
		Items: mapSlice(collaborators, func(c db.Collaborator) *tangled.RepoListCollaborators_ListItem {
			return &tangled.RepoListCollaborators_ListItem{
				Subject:   c.Subject.String(),
				AddedBy:   c.AddedBy.String(),
				CreatedAt: c.Created,
			}
		}),
	}
	if next != nil {
		cur := strconv.Itoa(*next)
		response.Cursor = &cur
	}

	x.writeJson(w, response)
}
