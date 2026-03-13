package repo

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"github.com/go-chi/chi/v5"
)

func (rp *Repo) DownloadArchive(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DownloadArchive")
	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)
	ref = strings.TrimSuffix(ref, ".tar.gz")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	scheme := "http"
	if !rp.config.Core.Dev {
		scheme = "https"
	}
	host := fmt.Sprintf("%s://%s", scheme, f.Knot)
	didSlashRepo := f.DidSlashRepo()

	// build the xrpc url
	u, err := url.Parse(host)
	if err != nil {
		l.Error("failed to parse host URL", "err", err)
		rp.pages.Error503(w)
		return
	}

	u.Path = "/xrpc/sh.tangled.repo.archive"
	query := url.Values{}
	query.Set("format", "tar.gz")
	query.Set("prefix", r.URL.Query().Get("prefix"))
	query.Set("ref", ref)
	query.Set("repo", didSlashRepo)
	u.RawQuery = query.Encode()

	xrpcURL := u.String()

	// make the get request
	resp, err := http.Get(xrpcURL)
	if err != nil {
		l.Error("failed to call XRPC repo.archive", "err", err)
		rp.pages.Error503(w)
		return
	}
	defer resp.Body.Close()

	// pass through headers from upstream response
	if contentDisposition := resp.Header.Get("Content-Disposition"); contentDisposition != "" {
		w.Header().Set("Content-Disposition", contentDisposition)
	}
	if contentType := resp.Header.Get("Content-Type"); contentType != "" {
		w.Header().Set("Content-Type", contentType)
	}
	if contentLength := resp.Header.Get("Content-Length"); contentLength != "" {
		w.Header().Set("Content-Length", contentLength)
	}
	if link := resp.Header.Get("Link"); link != "" {
		if resolvedRef, err := extractImmutableLink(link); err == nil {
			newLink := fmt.Sprintf("<%s/%s/archive/%s.tar.gz>; rel=\"immutable\"",
				rp.config.Core.BaseUrl(), f.DidSlashRepo(), resolvedRef)
			w.Header().Set("Link", newLink)
		}
	}

	// stream the archive data directly
	if _, err := io.Copy(w, resp.Body); err != nil {
		l.Error("failed to write response", "err", err)
	}
}

func extractImmutableLink(linkHeader string) (string, error) {
	trimmed := strings.TrimPrefix(linkHeader, "<")
	trimmed = strings.TrimSuffix(trimmed, ">; rel=\"immutable\"")

	parsedLink, err := url.Parse(trimmed)
	if err != nil {
		return "", err
	}

	resolvedRef := parsedLink.Query().Get("ref")
	if resolvedRef == "" {
		return "", fmt.Errorf("no ref found in link header")
	}

	return resolvedRef, nil
}
