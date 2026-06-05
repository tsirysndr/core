package xrpc

import (
	"crypto/sha256"
	"fmt"
	"io"
	"net/http"
	"path/filepath"
	"slices"
	"strings"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotmirror/xrpc/gitea"
)

func (x *Xrpc) GetBlob(w http.ResponseWriter, r *http.Request) {
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

	l := x.logger.With("method", "git.getBlob", "repo", repo, "ref", ref, "path", path)
	l.Debug("request")

	if path == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing path parameter"})
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
		l.Warn("local mirror failed", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get blob"})
		return
	}
	size, reader, err := gitea.ReadBlob(ctx, repoPath, entry.Hash)
	if err != nil {
		l.Warn("local mirror failed", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to get blob"})
		return
	}
	defer reader.Close()

	// default to octet-stream for large blobs
	if size > 1000*1000 { // 1MB
		w.Header().Set("Content-Type", "application/octet-stream")
		if _, err := io.Copy(w, reader); err != nil {
			l.Error("failed to serve the blob", "err", err)
		}
		return
	}

	contents, err := io.ReadAll(reader)
	if err != nil {
		l.Error("failed to read blob content", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "failed to read the blob"})
		return
	}

	eTag := fmt.Sprintf("\"%x\"", sha256.Sum256(contents))
	if clientETag := r.Header.Get("If-None-Match"); clientETag == eTag {
		w.WriteHeader(http.StatusNotModified)
		return
	}
	w.Header().Set("ETag", eTag)
	w.Header().Set("X-Content-Type-Options", "nosniff")

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

	switch {
	case strings.HasPrefix(mimeType, "image/"), strings.HasPrefix(mimeType, "video/"):
		w.Header().Set("Content-Type", mimeType)

	case strings.HasPrefix(mimeType, "text/") || isTextualMimeType(mimeType):
		w.Header().Set("Cache-Control", "public, no-cache")
		// serve all text content as text/plain
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")

	default:
		// fallback to octet-stream
		w.Header().Set("Content-Type", "application/octet-stream")
	}
	w.Write(contents)
}

var textualMimeTypes = []string{
	"application/json",
	"application/xml",
	"application/yaml",
	"application/x-yaml",
	"application/toml",
	"application/javascript",
	"application/ecmascript",
}

// isTextualMimeType returns true if the MIME type represents textual content
// that should be served as text/plain for security reasons
func isTextualMimeType(mimeType string) bool {
	return slices.Contains(textualMimeTypes, mimeType)
}
