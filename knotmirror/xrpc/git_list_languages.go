package xrpc

import (
	"context"
	"fmt"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

func (x *Xrpc) ListLanguages(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")
	)
	l := x.logger.With("method", "git.listLanguages", "repo", repoQuery, "ref", ref)
	l.Debug("request")

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		l.Error("invalid repo did", "err", err)
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	ctx := r.Context()

	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		l.Error("failed to make repo path", "err", err)
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "RepoNotFound", Message: fmt.Sprintf("unknown repository: %s", repo)})
		return
	}

	commit, err := gitea.GetCommit(ctx, repoPath, ref)
	if err != nil {
		l.Error("failed to get commit", "err", err)
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "RefNotFound", Message: fmt.Sprintf("unknown git ref: %s", repo)})
		return
	}

	indexCtx, cancel := context.WithTimeout(ctx, 1 * time.Second)
	defer cancel()
	sizes, err := x.indexer.IndexLanguages(indexCtx, repo, commit.Hash)
	if err != nil {
		l.Error("failed to serve languages", "err", err)
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to serve languages"})
		return
	}

	var out tangled.GitTempListLanguages_Output
	for lang, size := range sizes {
		out.Total += size
		out.Languages = append(out.Languages, &tangled.GitTempListLanguages_Language{
			Name: lang,
			Size: size,
		})
	}

	writeJson(w, http.StatusOK, &out)
}
