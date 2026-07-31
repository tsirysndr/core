package xrpc

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/knotmirror/xrpc/gitea"
	"tangled.org/core/types"
)

const fieldSeparator = "\x1f" // ASCII Unit Separator

func (x *Xrpc) ListBranches(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery   = r.URL.Query().Get("repo")
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

func (x *Xrpc) listBranches(ctx context.Context, repo syntax.DID, limit int, cursor int64) (*types.RepoBranchesResponse, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo did: %w", err)
	}

	// ignore error: an empty default branch just means nothing is marked default
	defaultBranch := func(repoPath string) string {
		out, err := exec.Command("git", "-C", repoPath, "rev-parse", "--abbrev-ref", "HEAD").Output()
		if err != nil {
			return ""
		}
		return string(bytes.TrimSpace(out))
	}(repoPath)

	// -> [](name, oid)
	type branchRef struct {
		name string
		oid  string
	}
	refs, err := func(repoPath string) ([]branchRef, error) {
		out, err := exec.Command(
			"git",
			"-C", repoPath,
			"for-each-ref",
			"--format=%(refname:short)"+fieldSeparator+"%(objectname)",
			"--sort=-creatordate",
			// for-each-ref has no skip flag, so fetch offset+limit and slice below
			fmt.Sprintf("--count=%d", cursor+int64(limit)),
			"refs/heads",
		).Output()
		if err != nil {
			return nil, err
		}

		out = bytes.TrimSpace(out)
		if len(out) == 0 {
			return nil, nil
		}
		lines := strings.Split(string(out), "\n")
		if int(cursor) >= len(lines) {
			return nil, nil
		}
		lines = lines[cursor:]

		refs := make([]branchRef, 0, len(lines))
		for _, line := range lines {
			name, oid, ok := strings.Cut(line, fieldSeparator)
			if !ok {
				continue
			}
			refs = append(refs, branchRef{name: name, oid: oid})
		}
		return refs, nil
	}(repoPath)
	if err != nil {
		return nil, fmt.Errorf("listing git branches: %w", err)
	}

	// hydrate each ref's commit
	branches, err := func(repoPath string, refs []branchRef) ([]types.Branch, error) {
		bw, br, cancel := gitea.CatFileBatch(ctx, repoPath)
		defer cancel()
		branches := make([]types.Branch, 0, len(refs))
		for _, ref := range refs {
			if _, err := bw.Write([]byte(ref.oid + "\n")); err != nil {
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
				return nil, fmt.Errorf("unexpected type: %s for commit id: %s", typ, ref.oid)
			}
			c, err := gitea.ReadCommit(plumbing.NewHash(ref.oid), io.LimitReader(br, size))
			if err != nil {
				return nil, err
			}
			if _, err := br.Discard(1); err != nil {
				return nil, err
			}
			branches = append(branches, types.Branch{
				IsDefault: ref.name == defaultBranch,
				Reference: types.Reference{
					Name: ref.name,
					Hash: ref.oid,
				},
				Commit: c,
			})
		}
		return branches, nil
	}(repoPath, refs)
	if err != nil {
		return nil, fmt.Errorf("hydrating branch commits: %w", err)
	}
	slices.Reverse(branches)

	// -> total
	total, err := func(repoPath string) (int, error) {
		out, err := exec.Command("git", "-C", repoPath, "for-each-ref", "--format=%(refname)", "refs/heads").Output()
		if err != nil {
			return 0, err
		}
		out = bytes.TrimSpace(out)
		if len(out) == 0 {
			return 0, nil
		}
		return bytes.Count(out, []byte{'\n'}) + 1, nil
	}(repoPath)
	if err != nil {
		return nil, fmt.Errorf("counting git branches: %w", err)
	}

	return &types.RepoBranchesResponse{
		Branches: branches,
		Total:    total,
	}, nil
}

func (x *Xrpc) makeRepoPath(ctx context.Context, repoDid syntax.DID) (string, error) {
	path := filepath.Join(x.cfg.GitRepoBasePath, repoDid.String())
	if _, err := os.Stat(path); err != nil {
		return "", fmt.Errorf("repo %s not mirrored locally: %w", repoDid, err)
	}
	return path, nil
}
