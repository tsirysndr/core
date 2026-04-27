package xrpc

import (
	"fmt"
	"net/http"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"

	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) RepoTag(w http.ResponseWriter, r *http.Request) {
	repo := r.URL.Query().Get("repo")
	repoPath, err := x.parseRepoParam(repo)
	if err != nil {
		writeError(w, err.(xrpcerr.XrpcError), http.StatusBadRequest)
		return
	}

	tagName := r.URL.Query().Get("tag")
	if tagName == "" {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("missing name parameter"),
		), http.StatusBadRequest)
		return
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		x.Logger.Error("failed to open", "error", err)
		writeError(w, xrpcerr.RepoNotFoundError, http.StatusNoContent)
		return
	}

	// if this is not already formatted as refs/tags/v0.1.0, then format it
	if !plumbing.ReferenceName(tagName).IsTag() {
		tagName = plumbing.NewTagReferenceName(tagName).String()
	}

	tags, err := gr.Tags(&git.TagsOptions{
		Pattern: tagName,
	})

	if len(tags) != 1 {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("TagNotFound"),
			xrpcerr.WithMessage(fmt.Sprintf("expected 1 tag to be returned, got %d tags", len(tags))),
		), http.StatusBadRequest)
		return
	}

	tag := tags[0]

	if err != nil {
		x.Logger.Warn("getting tags", "error", err.Error())
		tags = []object.Tag{}
	}

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

	response := types.RepoTagResponse{
		Tag: &tr,
	}

	x.writeJson(w, response)
}
