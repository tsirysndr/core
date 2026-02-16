package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"path/filepath"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/types"
)

func (x *Xrpc) ListBranches(w http.ResponseWriter, r *http.Request) {
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

	l := x.logger.With("repo", repoQuery, "limit", limit, "cursor", cursor)

	out, err := x.listBranches(r.Context(), repo, limit, cursor)
	if err != nil {
		// TODO: better error return
		l.Error("failed to list branches", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to list branches"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) listBranches(ctx context.Context, repo syntax.ATURI, limit int, cursor int64) (*types.RepoBranchesResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo at-uri: %w", err)
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		return nil, fmt.Errorf("opening git repo: %w", err)
	}

	branches, err := gr.Branches(&git.BranchesOptions{
		Limit:  limit,
		Offset: int(cursor),
	})
	if err != nil {
		return nil, fmt.Errorf("listing git branches: %w", err)
	}

	return &types.RepoBranchesResponse{
		// TODO: include default branch and cursor
		Branches: branches,
	}, nil
}

func (x *Xrpc) makeRepoPath(ctx context.Context, repo syntax.ATURI) (string, error) {
	id, err := x.resolver.ResolveIdent(ctx, repo.Authority().String())
	if err != nil {
		return "", err
	}

	return filepath.Join(
		x.cfg.GitRepoBasePath,
		id.DID.String(),
		repo.RecordKey().String(),
	), nil
}
