package main

import (
	"context"
	"crypto/sha1"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/samber/lo"
	"tangled.org/core/repoident"
	"tangled.org/core/repoverify"
)

// 1 MB; match https://sourcegraph.sourcegraph.com/r/github.com/sourcegraph/sourcegraph/-/blob/cmd/searcher/internal/search/store.go?L32
const MaxFileSize = 1 << 20

func gitIndex(ctx context.Context, cfg *Config, dir identity.Directory, req indexRequest) error {
	ctx, cancel := context.WithTimeout(ctx, cfg.IndexTimeout)
	defer cancel()

	repo, err := loadRepo(ctx, cfg, dir, req.Repo)
	if err != nil {
		return fmt.Errorf("loading repo %s: %w", req.Repo, err)
	}
	repo.Branches = req.Branches

	gitDir, err := tmpGitDir(repo.Did.String())
	if err != nil {
		return err
	}
	defer os.RemoveAll(gitDir) // best-effort cleanup

	if err := fetchRepo(ctx, gitDir, repo.CloneURL(), req.Branches); err != nil {
		return err
	}

	if err := indexRepo(ctx, cfg, gitDir, *repo); err != nil {
		return err
	}

	return nil
}

func loadRepo(ctx context.Context, cfg *Config, dir identity.Directory, repoDID repoident.RepoDid) (*Repo, error) {
	ident, err := dir.LookupDID(ctx, syntax.DID(repoDID))
	if err != nil {
		return nil, err
	}

	knot, err := repoident.KnotURLFromIdentity(ident, cfg.KnotScheme)
	if err != nil {
		return nil, fmt.Errorf("repoDid %s: %w", repoDID, err)
	}

	described, err := repoverify.Describe(ctx, nil, knot, repoDID)
	if err != nil {
		return nil, err
	}

	return &Repo{
		Did:   repoDID,
		Owner: described.OwnerDid,
		Slug:  described.Rkey,
		Knot:  knot,
	}, nil
}

func fetchRepo(ctx context.Context, gitDir, cloneUrl string, branches []indexBranch) error {
	// Create a repo to fetch into
	if err := executeCmd(ctx,
		"git",
		// use a random default branch. This is so that HEAD isn't a symref to a
		// branch that is indexed. For example if you are indexing
		// HEAD,master. Then HEAD would be pointing to master by default.
		"-c", "init.defaultBranch=nonExistentBranchBB0FOFCH32",
		"init",
		// we don't need a working copy
		"--bare",
		gitDir,
	); err != nil {
		return err
	}

	fetchArgs := append([]string{
		"-C", gitDir,
		"-c", "protocol.version=2",
		"fetch", "--depth=1", "--no-tags",
		// Git's blob:limit filter excludes blobs whose size is >= the given limit,
		// while zoekt indexes files up to and including FileLimit bytes.
		fmt.Sprintf("--filter=blob:limit=%d", int64(MaxFileSize)+1),
		cloneUrl,
	}, lo.Map(branches, func(b indexBranch, _ int) string { return string(b.Version) })...)

	if err := executeCmd(ctx, "git", fetchArgs...); err != nil {
		return err
	}

	return errors.Join(lo.FilterMap(branches, func(b indexBranch, _ int) (error, bool) {
		err := executeCmd(ctx, "git", "-C", gitDir, "update-ref", b.Name.Ref(), string(b.Version))
		if err == nil {
			return nil, false
		}
		return fmt.Errorf("failed update-ref %s to %s: %w", b.Name.Ref(), b.Version, err), true
	})...)
}

func indexRepo(ctx context.Context, cfg *Config, gitDir string, repo Repo) error {
	executablePath, err := os.Executable()
	if err != nil {
		return err
	}

	repoJson, err := json.Marshal(repo)
	if err != nil {
		return err
	}

	args := []string{"index"}
	args = append(args, "-index-dir", cfg.IndexDir)
	args = append(args, "-appview-url", cfg.AppviewUrl)
	args = append(args, gitDir, string(repoJson))
	if err := executeCmd(ctx, executablePath, args...); err != nil {
		return err
	}
	return nil
}

func tmpGitDir(name string) (string, error) {
	abs := url.QueryEscape(name)
	if len(abs) > 200 {
		h := sha1.New()
		_, _ = io.WriteString(h, abs)
		abs = abs[:200] + fmt.Sprintf("%x", h.Sum(nil))[:8]
	}
	dir := filepath.Join(os.TempDir(), abs+".git")
	if _, err := os.Stat(dir); err == nil {
		if err := os.RemoveAll(dir); err != nil {
			return "", err
		}
	}
	return dir, nil
}
