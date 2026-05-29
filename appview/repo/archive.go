package repo

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
)

func (rp *Repo) DownloadArchive(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DownloadArchive")
	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)
	format := r.URL.Query().Get("format")
	ref, format = archiveRefAndFormat(ref, format, r.UserAgent())
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	// build the xrpc url
	query := url.Values{}
	query.Set("repo", f.RepoDid)
	query.Set("ref", ref)
	query.Set("format", format)
	query.Set("prefix", r.URL.Query().Get("prefix"))
	xrpcURL := fmt.Sprintf(
		"%s/xrpc/%s?%s",
		rp.config.KnotMirror.Url,
		tangled.GitTempGetArchiveNSID,
		query.Encode(),
	)

	// make the get request
	resp, err := http.Get(xrpcURL)
	if err != nil {
		l.Error("failed to call XRPC repo.archive", "err", err)
		rp.pages.Error503(w)
		return
	}
	defer resp.Body.Close()

	w.Header().Set("Content-Type", archiveContentType(format))

	filename := ""
	if cd := resp.Header.Get("Content-Disposition"); strings.HasPrefix(cd, "attachment;") {
		filename = cd // knot has already set the attachment CD
	}
	if filename == "" {
		filename = fmt.Sprintf("attachment; filename=\"%s-%s.%s\"", f.Name, ref, format)
	}
	w.Header().Set("Content-Disposition", filename)
	w.Header().Set("X-Content-Type-Options", "nosniff")

	if link := resp.Header.Get("Link"); link != "" {
		if resolvedRef, err := extractImmutableLink(link); err == nil {
			newLink := fmt.Sprintf("<%s/%s/archive/%s.%s>; rel=\"immutable\"",
				rp.config.Core.BaseUrl(), f.RepoIdentifier(), resolvedRef, format)
			w.Header().Set("Link", newLink)
		}
	}

	// stream the archive data directly
	if _, err := io.Copy(w, resp.Body); err != nil {
		l.Error("failed to write response", "err", err)
	}
}

func archiveRefAndFormat(ref string, requestedFormat string, userAgent string) (string, string) {
	switch {
	case strings.HasSuffix(ref, ".tar.gz"):
		ref = strings.TrimSuffix(ref, ".tar.gz")
		if requestedFormat == "" {
			requestedFormat = "tar.gz"
		}
	case strings.HasSuffix(ref, ".zip"):
		ref = strings.TrimSuffix(ref, ".zip")
		if requestedFormat == "" {
			requestedFormat = "zip"
		}
	}

	switch requestedFormat {
	case "zip", "tar.gz":
		return ref, requestedFormat
	default:
		if prefersZipArchive(userAgent) {
			return ref, "zip"
		}
		return ref, "tar.gz"
	}
}

func prefersZipArchive(userAgent string) bool {
	ua := strings.ToLower(userAgent)
	return strings.Contains(ua, "windows") || strings.Contains(ua, "win64") || strings.Contains(ua, "win32")
}

func archiveContentType(format string) string {
	if format == "zip" {
		return "application/zip"
	}
	return "application/gzip"
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
