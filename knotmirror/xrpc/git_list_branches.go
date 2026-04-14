package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"path/filepath"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotmirror/db"
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

	out, err := x.listBranches(r.Context(), repo, limit, cursor)
	if err != nil {
		x.logger.Warn("local mirror failed, trying proxy", "repo", repo, "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
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
	r, err := db.GetRepoByAtUri(ctx, x.db, repo)
	if err != nil {
		return "", fmt.Errorf("looking up repo: %w", err)
	}
	if r == nil {
		return "", fmt.Errorf("repo not found: %s", repo)
	}
	if r.RepoDid == "" {
		return "", fmt.Errorf("repo missing repo_did: %s", repo)
	}
	return filepath.Join(x.cfg.GitRepoBasePath, r.RepoDid.String()), nil
}
