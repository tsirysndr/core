package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
)

// TODO: maybe rename to `sh.tangled.repo.temp.getCommit`?
// then, we should ensure the given `ref` is valid
func (x *Xrpc) GetBranch(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		nameQuery = r.URL.Query().Get("name")
	)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	if nameQuery == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing name parameter"})
		return
	}
	branchName, _ := url.PathUnescape(nameQuery)

	l := x.logger.With("repo", repo, "branch", branchName)

	out, err := x.getBranch(r.Context(), repo, branchName)
	if err != nil {
		// TODO: better error return
		l.Error("failed to get branch", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get branch"})
		return
	}
	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) getBranch(ctx context.Context, repo syntax.ATURI, branchName string) (*tangled.GitTempGetBranch_Output, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve repo at-uri: %w", err)
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repo: %w", err)
	}

	ref, err := gr.Branch(branchName)
	if err != nil {
		return nil, fmt.Errorf("getting branch '%s': %w", branchName, err)
	}

	commit, err := gr.Commit(ref.Hash())
	if err != nil {
		return nil, fmt.Errorf("getting commit '%s': %w", ref.Hash(), err)
	}

	out := tangled.GitTempGetBranch_Output{
		Name: ref.Name().Short(),
		Hash: ref.Hash().String(),
		When: commit.Author.When.Format(time.RFC3339),
		Author: &tangled.GitTempDefs_Signature{
			Name:  commit.Author.Name,
			Email: commit.Author.Email,
			When:  commit.Author.When.Format(time.RFC3339),
		},
	}

	if commit.Message != "" {
		out.Message = &commit.Message
	}

	return &out, nil
}
