package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
)

func (x *Xrpc) ListTags(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery   = r.URL.Query().Get("repo")
		limitQuery  = r.URL.Query().Get("limit")
		cursorQuery = r.URL.Query().Get("cursor")
	)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	limit := 50
	if limitQuery != "" {
		limit, err = strconv.Atoi(limitQuery)
		if err != nil || limit < 1 || limit > 1000 {
			writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("limit parameter invalid: %s", limitQuery)})
			return
		}
	}

	var cursor int64
	if cursorQuery != "" {
		cursor, err = strconv.ParseInt(cursorQuery, 10, 64)
		if err != nil || cursor < 0 {
			writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("cursor parameter invalid: %s", cursorQuery)})
			return
		}
	}

	l := x.logger.With("repo", repo, "limit", limit, "cursor", cursor)

	out, err := x.listTags(r.Context(), repo, limit, cursor)
	if err != nil {
		// TODO: better error return
		l.Error("failed to list tags", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to list tags"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) listTags(ctx context.Context, repo syntax.ATURI, limit int, cursor int64) (*types.RepoTagsResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve repo at-uri: %w", err)
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repo: %w", err)
	}

	tags, err := gr.Tags(&git.TagsOptions{
		Limit:  limit,
		Offset: int(cursor),
	})
	if err != nil {
		return nil, fmt.Errorf("failed to get git tags: %w", err)
	}

	rtags := make([]*types.TagReference, len(tags))
	for i, tag := range tags {
		var target *object.Tag
		if tag.Target != plumbing.ZeroHash {
			target = &tag
		}
		rtags[i] = &types.TagReference{
			Reference: types.Reference{
				Name: tag.Name,
				Hash: tag.Hash.String(),
			},
			Tag:     target,
			Message: tag.Message,
		}
	}

	return &types.RepoTagsResponse{
		Tags: rtags,
	}, nil
}
