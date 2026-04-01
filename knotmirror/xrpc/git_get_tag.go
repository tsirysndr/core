package xrpc

import (
	"context"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
)

func (x *Xrpc) GetTag(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		tagName   = r.URL.Query().Get("tag")
	)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	if tagName == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing 'tag' parameter"})
		return
	}

	out, err := x.getTag(r.Context(), repo, tagName)
	if err != nil {
		x.logger.Warn("local mirror failed, trying proxy", "repo", repo, "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get tag"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) getTag(ctx context.Context, repo syntax.ATURI, tagName string) (*types.RepoTagResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve repo at-uri: %w", err)
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repo: %w", err)
	}

	// if this is not already formatted as refs/tags/v0.1.0, then format it
	if !plumbing.ReferenceName(tagName).IsTag() {
		tagName = plumbing.NewTagReferenceName(tagName).String()
	}

	tag, err := func() (object.Tag, error) {
		tags, err := gr.Tags(&git.TagsOptions{
			Pattern: tagName,
		})
		if err != nil {
			return object.Tag{}, err
		}
		if len(tags) != 1 {
			return object.Tag{}, fmt.Errorf("expected 1 tag to be returned, got %d tags", len(tags))
		}
		return tags[0], nil
	}()
	if err != nil {
		return nil, fmt.Errorf("getting tag: %w", err)
	}

	var target *object.Tag
	if tag.Target != plumbing.ZeroHash {
		target = &tag
	}

	return &types.RepoTagResponse{
		Tag: &types.TagReference{
			Tag: target,
			Reference: types.Reference{
				Name: tag.Name,
				Hash: tag.Hash.String(),
			},
			Message: tag.Message,
		},
	}, nil
}
