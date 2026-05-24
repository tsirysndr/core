package xrpc

import (
	"fmt"
	"net/http"
	"net/url"
	"os/exec"
	"strings"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

func (x *Xrpc) GetArchive(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")
		format    = r.URL.Query().Get("format")
		prefix    = r.URL.Query().Get("prefix")
	)

	repo, err := syntax.ParseDID(repoQuery)
	if err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	if format != "tar.gz" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "only tar.gz format is supported"})
		return
	}
	if format == "" {
		format = "tar.gz"
	}

	l := x.logger.With("repo", repo, "ref", ref, "format", format, "prefix", prefix)
	l.Debug("request")

	ctx := r.Context()

	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to resolve repo"})
		return
	}

	rev := ref
	if rev == "" {
		rev = "HEAD"
	}
	commit, err := gitea.GetCommit(ctx, repoPath, rev)

	repoName, err := func() (string, error) {
		r, err := db.GetRepoByRepoDid(ctx, x.db, repo)
		if err != nil {
			return "", err
		}
		if r == nil {
			return "", fmt.Errorf("repo not found: %s", repo)
		}
		return r.Name, nil
	}()
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to retrieve repo name"})
		return
	}

	safeRefFilename := strings.ReplaceAll(plumbing.ReferenceName(ref).Short(), "/", "-")
	immutableLink := func() string {
		params := url.Values{}
		params.Set("repo", repo.String())
		params.Set("ref", commit.Hash.String())
		params.Set("format", format)
		params.Set("prefix", prefix)
		return fmt.Sprintf("%s/xrpc/%s?%s", x.cfg.BaseUrl(), tangled.GitTempGetArchiveNSID, params.Encode())
	}()

	filename := fmt.Sprintf("%s-%s.tar.gz", repoName, safeRefFilename)
	w.Header().Set("Content-Disposition", fmt.Sprintf("attachment; filename=\"%s\"", filename))
	w.Header().Set("Content-Type", "application/gzip")
	w.Header().Set("Link", fmt.Sprintf("<%s>; rel=\"immutable\"", immutableLink))

	cmd := exec.Command(
		"git",
		"archive",
		fmt.Sprintf("--prefix=%s", prefix),
		"--format=tar.gz",
		commit.Hash.String(),
	)

	var stderr strings.Builder
	cmd.Dir = repoPath
	cmd.Stdout = w
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		err = fmt.Errorf("%w\n%s", err, stderr.String())
		l.Error("failed to archive", "err", err)
		w.WriteHeader(http.StatusInternalServerError)
	}
}
