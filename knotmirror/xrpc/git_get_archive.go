package xrpc

import (
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/gitutil"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

func (x *Xrpc) GetArchive(w http.ResponseWriter, r *http.Request) {
	invalid := func(err error) {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "InvalidRequest", Message: err.Error()})
	}

	repoQuery := r.URL.Query().Get("repo")
	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		invalid(fmt.Errorf("repo parameter invalid: %s", repoQuery))
		return
	}

	params, err := gitutil.ParseArchiveParams(r.URL.Query())
	if err != nil {
		invalid(err)
		return
	}

	l := x.logger.With("repo", repo, "ref", params.Rev, "format", params.Format, "prefix", params.Prefix)
	l.Debug("request")

	ctx := r.Context()
	proxy := func(err error, message string) {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if !x.proxyToKnot(w, r, repo) {
			writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: message})
		}
	}

	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		proxy(err, "failed to resolve repo")
		return
	}

	commit, err := gitea.GetCommit(ctx, repoPath, params.Rev.Or(gitutil.RevHead).String())
	if err != nil {
		proxy(err, "failed to resolve ref")
		return
	}

	mirrored, err := db.GetRepoByRepoDid(ctx, x.db, repo)
	if err == nil && mirrored == nil {
		err = fmt.Errorf("repo not found: %s", repo)
	}
	if err != nil {
		proxy(err, "failed to retrieve repo name")
		return
	}

	hash := gitutil.RevFromHash(commit.Hash)
	served := params.WithRev(params.Rev.Or(hash)).Serve(gitutil.RepoName(mirrored.Name)).WithRev(hash)

	served.SetHeaders(w.Header())
	w.Header().Set("Link", gitutil.ImmutableLink(fmt.Sprintf("%s/xrpc/%s?%s",
		x.cfg.BaseUrl(), tangled.GitTempGetArchiveNSID, served.Query(repo.String()).Encode(),
	)))
	if served.ServeNotModified(w, r, gitutil.RepoIdentity(repo.String())) {
		return
	}

	body := gitutil.NewResponseBody(w)
	if err := gitutil.WriteArchive(ctx, body, repoPath, served); err != nil {
		l.Error("writing archive", "err", err.Error(), "format", params.Format)
		body.Fail()
	}
}
