package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"path/filepath"
	"time"
	"unicode/utf8"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/knotserver/git"
)

func (x *Xrpc) GetTree(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")  // ref can be empty (git.Open handles this)
		path      = r.URL.Query().Get("path") // path can be empty (defaults to root)
	)
	l := x.logger.With("method", "git.getTree", "repo", repoQuery, "ref", ref)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	out, err := x.getTree(r.Context(), repo, ref, path)
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

func (x *Xrpc) getTree(ctx context.Context, repo syntax.ATURI, ref, path string) (*tangled.GitTempGetTree_Output, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve repo at-uri: %w", err)
	}

	gr, err := git.Open(repoPath, ref)
	if err != nil {
		return nil, fmt.Errorf("opening git repo: %w", err)
	}

	files, err := gr.FileTree(ctx, path)
	if err != nil {
		return nil, fmt.Errorf("reading file tree: %w", err)
	}

	// if any of these files are a readme candidate, pass along its blob contents too
	var readmeFileName string
	var readmeContents string
	for _, file := range files {
		if markup.IsReadmeFile(file.Name) {
			contents, err := gr.RawContent(filepath.Join(path, file.Name))
			if err != nil {
				x.logger.Error("failed to read contents of file", "path", path, "file", file.Name)
			}

			if utf8.Valid(contents) {
				readmeFileName = file.Name
				readmeContents = string(contents)
				break
			}
		}
	}

	// convert NiceTree -> tangled.RepoTempGetTree_TreeEntry
	treeEntries := make([]*tangled.GitTempGetTree_TreeEntry, len(files))
	for i, file := range files {
		entry := &tangled.GitTempGetTree_TreeEntry{
			Name: file.Name,
			Mode: file.Mode,
			Size: file.Size,
		}
		if file.LastCommit != nil {
			entry.Last_commit = &tangled.GitTempGetTree_LastCommit{
				Hash:    file.LastCommit.Hash.String(),
				Message: file.LastCommit.Message,
				When:    file.LastCommit.When.Format(time.RFC3339),
			}
		}
		treeEntries[i] = entry
	}

	var parentPtr *string
	if path != "" {
		parentPtr = &path
	}

	var dotdotPtr *string
	if path != "" {
		dotdot := filepath.Dir(path)
		if dotdot != "." {
			dotdotPtr = &dotdot
		}
	}

	return &tangled.GitTempGetTree_Output{
		Ref:    ref,
		Parent: parentPtr,
		Dotdot: dotdotPtr,
		Files:  treeEntries,
		Readme: &tangled.GitTempGetTree_Readme{
			Filename: readmeFileName,
			Contents: readmeContents,
		},
	}, nil
}
