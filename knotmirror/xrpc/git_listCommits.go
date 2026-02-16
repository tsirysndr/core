package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
)

func (x *Xrpc) ListCommits(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery   = r.URL.Query().Get("repo")
		ref         = r.URL.Query().Get("ref") // ref can be empty (git.Open handles this)
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

	l := x.logger.With("repo", repo, "ref", ref)

	out, err := x.listCommits(r.Context(), repo, ref, limit, cursor)
	if err != nil {
		// TODO: better error return
		l.Error("failed to list commits", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to list commits"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) listCommits(ctx context.Context, repo syntax.ATURI, ref string, limit int, cursor int64) (*types.RepoLogResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo at-uri: %w", err)
	}

	gr, err := git.Open(repoPath, ref)
	if err != nil {
		return nil, fmt.Errorf("opening git repo: %w", err)
	}

	offset := int(cursor)

	commits, err := gr.Commits(offset, limit)
	if err != nil {
		return nil, fmt.Errorf("listing git commits: %w", err)
	}

	tcommits := make([]types.Commit, len(commits))
	for i, c := range commits {
		tcommits[i].FromGoGitCommit(c)
	}

	total, err := gr.TotalCommits()
	if err != nil {
		return nil, fmt.Errorf("counting total commits: %w", err)
	}

	return &types.RepoLogResponse{
		Commits: tcommits,
		Ref:     ref,
		Page:    (offset / limit) + 1,
		PerPage: limit,
		Total:   total,
		Log:     true,
	}, nil
}
