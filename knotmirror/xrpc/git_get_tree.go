package xrpc

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"path/filepath"
	"time"
	"unicode/utf8"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/api/tangled"
	"tangled.org/core/gitutil"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

const (
	LastCommitCache    = "last_commit:%s:%s"
	LastCommitCacheTTL = 30 * 24 * time.Hour
	MaxReadmeBytes     = 1 << 20
)

func (x *Xrpc) GetTree(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")  // ref can be empty (git.Open handles this)
		path      = r.URL.Query().Get("path") // path can be empty (defaults to root)
	)
	l := x.logger.With("method", "git.getTree", "repo", repoQuery, "ref", ref)
	l.Debug("request")

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	var out *tangled.GitTempGetTree_Output
	out, err = x.getTree(r.Context(), repo, ref, path)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "repo", repo, "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get tree"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) getTree(ctx context.Context, repo syntax.DID, ref, treePath string) (*tangled.GitTempGetTree_Output, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve repo did: %w", err)
	}
	rev := ref
	if rev == "" {
		rev = "HEAD"
	}

	head, err := gitea.GetCommit(ctx, repoPath, rev)
	if err != nil {
		return nil, fmt.Errorf("get head commit: %w", err)
	}

	subRev := head.Hash.String() + "^{tree}"
	if treePath != "" {
		subRev = head.Hash.String() + ":" + treePath
	}
	subTree, err := gitea.GetTree(ctx, repoPath, subRev)
	if err != nil {
		return nil, fmt.Errorf("get subtree %s: %w", subRev, err)
	}

	entryPaths := make([]string, len(subTree.Entries)+1)
	entryPaths[0] = ""
	for i, entry := range subTree.Entries {
		entryPaths[i+1] = entry.Name
	}

	commits, lastCommit, err := func(ctx context.Context, commit *object.Commit, treePath string, paths []string) (map[string]*object.Commit, *object.Commit, error) {
		headRef := commit.Hash.String()

		revs := make(map[string]string, len(paths))
		var unHitPaths []string

		keys := make([]string, len(paths))
		for i, path := range paths {
			keys[i] = fmt.Sprintf(LastCommitCache, headRef, filepath.Join(treePath, path))
		}
		if cached, err := x.rdb.MGet(ctx, keys...).Result(); err == nil {
			for i, v := range cached {
				if s, ok := v.(string); ok && s != "" {
					revs[paths[i]] = s
				} else {
					unHitPaths = append(unHitPaths, paths[i])
				}
			}
		} else {
			unHitPaths = paths
		}

		if len(unHitPaths) > 0 {
			commits, err := gitea.WalkGitLog(ctx, repoPath, headRef, treePath, unHitPaths...)
			if err != nil {
				return nil, nil, err
			}
			pipe := x.rdb.Pipeline()
			for path, cid := range commits {
				if cid == "" {
					continue
				}
				revs[path] = cid
				pipe.Set(ctx, fmt.Sprintf(LastCommitCache, headRef, filepath.Join(treePath, path)), cid, LastCommitCacheTTL)
			}
			if _, err := pipe.Exec(ctx); err != nil {
				x.logger.Warn("git last-commit cache write failed", "err", err)
			}
		}

		// start cat-file batch
		batchWriter, batchReader, cancel := gitea.CatFileBatch(ctx, repoPath)
		defer cancel()

		// path -> commit map
		commitsMap := map[string]*object.Commit{}
		for path, commitId := range revs {
			if commitId == headRef {
				commitsMap[path] = commit
				continue
			}

			if commitId == "" { // invalid commit?
				continue
			}

			_, err := batchWriter.Write([]byte(commitId + "\n"))
			if err != nil {
				return nil, nil, err
			}
			_, typ, size, err := gitea.ReadBatchLine(batchReader)
			if err != nil {
				return nil, nil, err
			}
			if typ != "commit" {
				if err := gitea.DiscardFull(batchReader, size+1); err != nil {
					return nil, nil, err
				}
				return nil, nil, fmt.Errorf("unexpected type: %s for commit id: %s", typ, commitId)
			}
			c, err := gitea.ReadCommit(plumbing.NewHash(commitId), io.LimitReader(batchReader, size))
			if _, err := batchReader.Discard(1); err != nil {
				return nil, nil, err
			}
			commitsMap[path] = c
		}

		var treeCommit *object.Commit
		if treePath == "" {
			treeCommit = commit
		} else if c, ok := commitsMap[""]; ok {
			treeCommit = c
		}

		return commitsMap, treeCommit, nil
	}(ctx, head, treePath, entryPaths)
	if err != nil {
		return nil, err
	}

	sizes, err := gitea.EntrySizes(ctx, repoPath, subTree.Entries)
	if err != nil {
		x.logger.Warn("tree entry size read failed", "err", err)
	}

	outEntries := make([]*tangled.GitTempGetTree_TreeEntry, len(subTree.Entries))
	for i, entry := range subTree.Entries {
		var entryLastCommit *tangled.GitTempGetTree_LastCommit
		if commit, ok := commits[entry.Name]; ok {
			entryLastCommit = &tangled.GitTempGetTree_LastCommit{
				Hash:    commit.Hash.String(),
				Message: commit.Message,
				When:    commit.Author.When.Format(time.RFC3339),
				Author: &tangled.GitTempGetTree_Signature{
					Email: commit.Author.Email,
					Name:  commit.Author.Name,
				},
			}
		}
		outEntries[i] = &tangled.GitTempGetTree_TreeEntry{
			Name:        entry.Name,
			Mode:        entry.Mode.String(),
			Size:        sizes[i],
			Last_commit: entryLastCommit,
		}
	}

	var parent *string
	var dotdot *string
	if treePath != "" {
		parent = &treePath
		if dir := filepath.Dir(treePath); dir != "." {
			dotdot = &dir
		}
	}

	var outLastCommit *tangled.GitTempGetTree_LastCommit
	if lastCommit != nil {
		outLastCommit = &tangled.GitTempGetTree_LastCommit{
			Hash:    lastCommit.Hash.String(),
			Message: lastCommit.Message,
			When:    lastCommit.Author.When.Format(time.RFC3339),
			Author: &tangled.GitTempGetTree_Signature{
				Email: lastCommit.Author.Email,
				Name:  lastCommit.Author.Name,
			},
		}
	}

	readmeName, readmeContents := x.readme(ctx, repoPath, subTree.Entries, sizes)

	return &tangled.GitTempGetTree_Output{
		Ref:        ref,
		Parent:     parent,
		Dotdot:     dotdot,
		Files:      outEntries,
		LastCommit: outLastCommit,
		// TODO: remove this field entirely
		Readme: &tangled.GitTempGetTree_Readme{
			Filename: readmeName,
			Contents: readmeContents,
		},
	}, nil
}

func (x *Xrpc) readme(ctx context.Context, repoPath string, entries []object.TreeEntry, sizes []int64) (string, string) {
	for i, entry := range entries {
		if !gitutil.IsReadmeFile(entry.Name, entry.Mode.String()) || sizes[i] > MaxReadmeBytes {
			continue
		}
		size, reader, err := gitea.ReadBlob(ctx, repoPath, entry.Hash)
		if err != nil {
			x.logger.Warn("readme blob open failed", "file", entry.Name, "err", err)
			continue
		}
		if size > MaxReadmeBytes {
			reader.Close()
			continue
		}
		contents, err := io.ReadAll(reader)
		reader.Close()
		if err != nil {
			x.logger.Warn("readme blob read failed", "file", entry.Name, "err", err)
			continue
		}
		if utf8.Valid(contents) {
			return entry.Name, string(contents)
		}
	}
	return "", ""
}
