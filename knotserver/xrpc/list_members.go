package xrpc

import (
	"net/http"
	"strconv"

	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) ListMembers(w http.ResponseWriter, r *http.Request) {
	p, err := parseListParams(r)
	if err != nil {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage(err.Error()),
		), http.StatusBadRequest)
		return
	}

	members, next, err := db.ListKnotMembers(x.Db, p)
	if err != nil {
		x.Logger.Error("failed to list knot members", "error", err)
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InternalServerError"),
			xrpcerr.WithMessage("failed to list knot members"),
		), http.StatusInternalServerError)
		return
	}

	response := tangled.KnotListMembers_Output{
		Items: mapSlice(members, func(m db.KnotMember) *tangled.KnotListMembers_ListItem {
			return &tangled.KnotListMembers_ListItem{
				Subject:   m.Subject.String(),
				AddedBy:   m.Did.String(),
				CreatedAt: m.Created,
			}
		}),
	}
	if next != nil {
		cur := strconv.Itoa(*next)
		response.Cursor = &cur
	}

	x.writeJson(w, response)
}
