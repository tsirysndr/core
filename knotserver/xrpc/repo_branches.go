package xrpc

import (
	"net/http"
	"strconv"

	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) RepoBranches(w http.ResponseWriter, r *http.Request) {
	repo := r.URL.Query().Get("repo")
	repoPath, err := x.parseRepoParam(repo)
	if err != nil {
		writeError(w, err.(xrpcerr.XrpcError), http.StatusBadRequest)
		return
	}

	// default
	limit := 50
	offset := 0

	if l, err := strconv.Atoi(r.URL.Query().Get("limit")); err == nil && l > 0 && l <= 100 {
		limit = l
	}

	if o, err := strconv.Atoi(r.URL.Query().Get("cursor")); err == nil && o > 0 {
		offset = o
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		writeError(w, xrpcerr.RepoNotFoundError, http.StatusNoContent)
		return
	}

	branches, _ := gr.Branches(&git.BranchesOptions{
		Limit:  limit,
		Offset: offset,
	})

	// Create response using existing types.RepoBranchesResponse
	response := types.RepoBranchesResponse{
		Branches: branches,
	}

	x.writeJson(w, response)
}
