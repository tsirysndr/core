package xrpc

import (
	"net/http"
	"strconv"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"

	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) RepoTags(w http.ResponseWriter, r *http.Request) {
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
		x.Logger.Error("failed to open", "error", err)
		writeError(w, xrpcerr.RepoNotFoundError, http.StatusNoContent)
		return
	}

	tags, err := gr.Tags(&git.TagsOptions{
		Limit:  limit,
		Offset: offset,
	})

	if err != nil {
		x.Logger.Warn("getting tags", "error", err.Error())
		tags = []object.Tag{}
	}

	rtags := []*types.TagReference{}
	for _, tag := range tags {
		var target *object.Tag
		if tag.Target != plumbing.ZeroHash {
			target = &tag
		}
		tr := types.TagReference{
			Tag: target,
		}

		tr.Reference = types.Reference{
			Name: tag.Name,
			Hash: tag.Hash.String(),
		}

		if tag.Message != "" {
			tr.Message = tag.Message
		}

		rtags = append(rtags, &tr)
	}

	response := types.RepoTagsResponse{
		Tags: rtags,
	}

	x.writeJson(w, response)
}
