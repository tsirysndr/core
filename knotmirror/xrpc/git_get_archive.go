package xrpc

import (
	"bytes"
	"context"
	"fmt"
	"io"
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

	if format == "" {
		format = "tar.gz"
	}
	if format != "tar.gz" && format != "zip" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "only tar.gz and zip formats are supported"})
		return
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
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to resolve ref"})
		return
	}

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
	if safeRefFilename == "" {
		safeRefFilename = commit.Hash.String()
	}
	immutableLink := func() string {
		params := url.Values{}
		params.Set("repo", repo.String())
		params.Set("ref", commit.Hash.String())
		params.Set("format", format)
		params.Set("prefix", prefix)
		return fmt.Sprintf("%s/xrpc/%s?%s", x.cfg.BaseUrl(), tangled.GitTempGetArchiveNSID, params.Encode())
	}()

	var archivePrefix string
	if prefix != "" {
		archivePrefix = prefix
	} else {
		archivePrefix = fmt.Sprintf("%s-%s", repoName, safeRefFilename)
	}

	filename := fmt.Sprintf("%s-%s.%s", repoName, safeRefFilename, format)
	w.Header().Set("Content-Disposition", fmt.Sprintf("attachment; filename=\"%s\"", filename))
	w.Header().Set("Content-Type", archiveContentType(format))
	w.Header().Set("Link", fmt.Sprintf("<%s>; rel=\"immutable\"", immutableLink))

	if err := writeLocalArchive(ctx, w, repoPath, commit.Hash.String(), format, archivePrefix); err != nil {
		l.Error("writing archive", "err", err.Error(), "format", format)
		w.WriteHeader(http.StatusInternalServerError)
	}
}

func archiveContentType(format string) string {
	if format == "zip" {
		return "application/zip"
	}
	return "application/gzip"
}

func writeLocalArchive(ctx context.Context, w io.Writer, repoPath, rev, format, prefix string) error {
	args := []string{"-C", repoPath, "archive", "--format=" + format}
	if prefix != "" {
		args = append(args, "--prefix="+strings.TrimRight(prefix, "/")+"/")
	}
	args = append(args, rev)

	cmd := exec.CommandContext(ctx, "git", args...)
	cmd.Stdout = w
	stderr := new(bytes.Buffer)
	cmd.Stderr = stderr

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%w, stderr: %s", err, stderr.String())
	}
	return nil
}
