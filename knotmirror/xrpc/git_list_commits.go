package xrpc

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"os/exec"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/knotmirror/xrpc/gitea"
	"tangled.org/core/types"
)

func (x *Xrpc) ListCommits(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery   = r.URL.Query().Get("repo")
		ref         = r.URL.Query().Get("ref") // ref can be empty (git.Open handles this)
		limitQuery  = r.URL.Query().Get("limit")
		cursorQuery = r.URL.Query().Get("cursor")
	)

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
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

	out, err := x.listCommits(r.Context(), repo, ref, limit, cursor)
	if err != nil {
		x.logger.Warn("local mirror failed, trying proxy", "repo", repo, "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to list commits"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) listCommits(ctx context.Context, repo syntax.DID, ref string, limit int, cursor int64) (*types.RepoLogResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo did: %w", err)
	}
	rev := ref
	if rev == "" {
		rev = "HEAD"
	}

	// -> []hash
	logs, err := func(repoPath, rev string) ([]byte, error) {
		out, err := exec.Command(
			"git",
			"-C", repoPath,
			"rev-list",
			rev,
			fmt.Sprintf("--skip=%d", cursor),
			fmt.Sprintf("--max-count=%d", limit),
		).Output()
		if err != nil {
			return nil, err
		}
		return bytes.TrimSpace(out), nil
	}(repoPath, rev)
	if err != nil {
		return nil, fmt.Errorf("reading rev-list: %w", err)
	}

	commits, err := func(repoPath string, logs []byte) ([]*object.Commit, error) {
		bw, br, cancel := gitea.CatFileBatch(ctx, repoPath)
		defer cancel()
		var commits []*object.Commit
		for commitId := range bytes.SplitSeq(logs, []byte{'\n'}) {
			_, err := bw.Write([]byte(string(commitId) + "\n"))
			if err != nil {
				return nil, err
			}
			_, typ, size, err := gitea.ReadBatchLine(br)
			if err != nil {
				return nil, err
			}
			if typ != "commit" {
				if err := gitea.DiscardFull(br, size+1); err != nil {
					return nil, err
				}
				return nil, fmt.Errorf("unexpected type: %s for commit id: %s", typ, commitId)
			}
			c, err := gitea.ReadCommit(plumbing.NewHash(string(commitId)), io.LimitReader(br, size))
			if err != nil {
				return nil, err
			}
			if _, err := br.Discard(1); err != nil {
				return nil, err
			}
			commits = append(commits, c)
		}
		return commits, nil
	}(repoPath, logs)
	if err != nil {
		return nil, fmt.Errorf("parsing commits: %w", err)
	}

	// -> total
	total, err := func(repoPath, rev string) (int, error) {
		out, err := exec.Command(
			"git",
			"-C", repoPath,
			"rev-list",
			rev,
			"--count",
		).Output()
		if err != nil {
			return 0, err
		}
		count, err := strconv.Atoi(string(bytes.TrimSpace(out)))
		if err != nil {
			return 0, err
		}
		return count, nil
	}(repoPath, rev)
	if err != nil {
		return nil, fmt.Errorf("parsing total commits: %w", err)
	}

	tcommits := make([]types.Commit, len(commits))
	for i, commit := range commits {
		tcommits[i].FromGoGitCommit(commit)
	}

	return &types.RepoLogResponse{
		Commits: tcommits,
		Ref:     ref,
		Page:    (int(cursor) / limit) + 1,
		Total:   total,
	}, nil
}
