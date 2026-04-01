package xrpc

import (
	"compress/gzip"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotserver/git"
)

func (x *Xrpc) GetArchive(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")
		format    = r.URL.Query().Get("format")
		prefix    = r.URL.Query().Get("prefix")
	)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
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

	gr, err := git.Open(repoPath, ref)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to open git repo"})
		return
	}

	repoName, err := func() (string, error) {
		r, err := db.GetRepoByAtUri(ctx, x.db, repo)
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
		params.Set("ref", gr.Hash().String())
		params.Set("format", format)
		params.Set("prefix", prefix)
		return fmt.Sprintf("%s/xrpc/%s?%s", x.cfg.BaseUrl(), tangled.GitTempGetArchiveNSID, params.Encode())
	}()

	filename := fmt.Sprintf("%s-%s.tar.gz", repoName, safeRefFilename)
	w.Header().Set("Content-Disposition", fmt.Sprintf("attachment; filename=\"%s\"", filename))
	w.Header().Set("Content-Type", "application/gzip")
	w.Header().Set("Link", fmt.Sprintf("<%s>; rel=\"immutable\"", immutableLink))

	gw := gzip.NewWriter(w)
	defer gw.Close()

	if err := gr.WriteTar(gw, prefix); err != nil {
		// once we start writing to the body we can't report error anymore
		// so we are only left with logging the error
		l.Error("writing tar file", "err", err.Error())
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	if err := gw.Flush(); err != nil {
		// once we start writing to the body we can't report error anymore
		// so we are only left with logging the error
		l.Error("flushing", "err", err.Error())
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
}
