package xrpc

import (
	"cmp"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

func (x *Xrpc) GetEntry(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = cmp.Or(r.URL.Query().Get("ref"), "HEAD")
		path      = r.URL.Query().Get("path")
	)

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	if path == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing path parameter"})
		return
	}

	l := x.logger.With("method", "git.getEntry", "repo", repo, "ref", ref, "path", path)
	l.Debug("request")

	ctx := r.Context()

	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "RepoNotFound", Message: fmt.Sprintf("unknown repository: %q", repo)})
		return
	}

	// resolve ref
	commit, err := gitea.GetCommit(ctx, repoPath, ref)
	if err != nil {
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "RefNotFound", Message: fmt.Sprintf("unknown ref: %q", ref)})
		return
	}
	ref = commit.Hash.String()

	entry, err := gitea.GetEntryFromCommit(ctx, repoPath, commit, path)
	if err != nil {
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "EntryNotFound", Message: fmt.Sprintf("entry %q not found", path)})
		return
	}
	size, err := gitea.GetBlobSize(ctx, repoPath, entry.Hash)
	if err != nil {
		l.Error("failed to read blob size", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to read blob size"})
		return
	}

	var outLastCommit *tangled.GitTempDefs_Commit
	var outSubmodule *tangled.GitTempDefs_Submodule

	lastCommit, err := gitea.GetCommitByPathWithID(ctx, commit.Hash, repoPath, path)
	if err != nil {
		l.Error("failed to find last commit", "err", err, "repoPath", repoPath)
	} else {
		outLastCommit = &tangled.GitTempDefs_Commit{
			Hash: refString(lastCommit.Hash.String()),
			Tree: refString(lastCommit.TreeHash.String()),
			Author: &tangled.GitTempDefs_Signature{
				Name:  lastCommit.Author.Name,
				Email: lastCommit.Author.Email,
				When:  lastCommit.Author.When.Format(time.RFC3339),
			},
			Committer: &tangled.GitTempDefs_Signature{
				Name:  lastCommit.Committer.Name,
				Email: lastCommit.Committer.Email,
				When:  lastCommit.Committer.When.Format(time.RFC3339),
			},
			Message: lastCommit.Message,
		}
	}

	if entry.Mode == filemode.Submodule {
		modules, err := gitea.GetSubmodules(ctx, repoPath, ref)
		if err != nil && !(errors.Is(err, gitea.ErrMissingGitModules) || errors.Is(err, gitea.ErrInvalidGitModules)) {
			writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to read .gitmodules"})
			return
		}

		if modules != nil {
			for _, submodule := range modules.Submodules {
				if submodule.Path == path {
					outSubmodule = &tangled.GitTempDefs_Submodule{
						Name:   submodule.Name,
						Url:    submodule.URL,
						Branch: refOptionalString(submodule.Branch),
					}
					break
				}
			}
		}
	}

	writeJson(w, http.StatusOK, tangled.GitTempGetEntry_Output{
		Name:       entry.Name,
		Mode:       entry.Mode.String(),
		Oid:        entry.Hash.String(),
		Size:       size,
		LastCommit: outLastCommit,
		Submodule:  outSubmodule,
	})
}

func refString(s string) *string { return &s }
func refOptionalString(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}
