package xrpc

import (
	"context"
	"encoding/base64"
	"fmt"
	"io"
	"net/http"
	"path/filepath"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/xrpc/gitea"
	"tangled.org/core/knotserver/git"
)

// TODO(boltless): rewrite lexicon in new NSID
func (x *Xrpc) RepoBlob(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref") // ref can be empty (git.Open handles this)
		path      = r.URL.Query().Get("path")
	)

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	l := x.logger.With("method", "repo.blob", "repo", repo, "ref", ref, "path", path)

	if path == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing path parameter"})
		return
	}

	gr, err := x.getRepo(r.Context(), repo, ref)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get blob"})
		return
	}

	ctx := r.Context()

	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "RepoNotFound", Message: fmt.Sprintf("unknown repository: %s", repo)})
		return
	}

	entry, err := gitea.GetEntry(ctx, repoPath, ref, path)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get blob"})
		return
	}

	if entry.Mode == filemode.Submodule {
		submodule, err := gr.Submodule(path)
		if err != nil {
			l.Warn("failed to load submodule", "err", err)
			writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to load submodule"})
			return
		}
		writeJson(w, http.StatusOK, tangled.RepoBlob_Output{
			Ref:  ref,
			Path: path,
			Submodule: &tangled.RepoBlob_Submodule{
				Name:   submodule.Name,
				Url:    submodule.URL,
				Branch: &submodule.Branch,
			},
		})
		return
	}

	size, reader, err := gitea.ReadBlob(ctx, repoPath, entry.Hash)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get blob"})
		return
	}
	defer reader.Close()

	if size > 1000*1000 { // 1MB
		fileTooLarge := true
		writeJson(w, http.StatusOK, tangled.RepoBlob_Output{
			Ref:          ref,
			Path:         path,
			Size:         &size,
			FileTooLarge: &fileTooLarge,
		})
		return
	}

	contents, err := io.ReadAll(reader)
	if err != nil {
		l.Error("failed to read blob content", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to read the blob"})
		return
	}

	mimeType := http.DetectContentType(contents)
	// override MIME types for formats that http.DetectContentType does not recognize
	switch filepath.Ext(path) {
	case ".svg":
		mimeType = "image/svg+xml"
	case ".avif":
		mimeType = "image/avif"
	case ".jxl":
		mimeType = "image/jxl"
	case ".heic", ".heif":
		mimeType = "image/heif"
	}

	isBinary := !(strings.HasPrefix(mimeType, "text/") || isTextualMimeType(mimeType))

	// include content for text blob or svg
	var content *string
	if !isBinary {
		content = new(string)
		*content = string(contents)
	} else if filepath.Ext(path) == ".svg" {
		content = new(string)
		*content = base64.StdEncoding.EncodeToString(contents)
	}

	response := tangled.RepoBlob_Output{
		Ref:      ref,
		Path:     path,
		Size:     &size,
		IsBinary: &isBinary,
		Content:  content,
	}

	ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
	defer cancel()

	lastCommit, err := gr.LastCommitFile(ctx, path)
	if err == nil && lastCommit != nil {
		response.LastCommit = &tangled.RepoBlob_LastCommit{
			Hash:    lastCommit.Hash.String(),
			Message: lastCommit.Message,
			When:    lastCommit.When.Format(time.RFC3339),
		}

		// try to get author information
		commit, err := gr.Commit(lastCommit.Hash)
		if err == nil {
			response.LastCommit.Author = &tangled.RepoBlob_Signature{
				Name:  commit.Author.Name,
				Email: commit.Author.Email,
			}
		}
	}

	writeJson(w, http.StatusOK, response)
}

func (x *Xrpc) getRepo(ctx context.Context, repo syntax.DID, ref string) (*git.GitRepo, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo did: %w", err)
	}

	gr, err := git.Open(repoPath, ref)
	if err != nil {
		return nil, fmt.Errorf("opening git repo: %w", err)
	}

	return gr, nil
}
