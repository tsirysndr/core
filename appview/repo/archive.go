package repo

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/samber/lo"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/gitutil"
)

const archiveRoute = "/archive/*"

func newArchiveClient(headerTimeout time.Duration) *http.Client {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.ResponseHeaderTimeout = headerTimeout
	return &http.Client{Transport: transport}
}

func (rp *Repo) DownloadArchive(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DownloadArchive")
	fail := func(status int) {
		w.WriteHeader(status)
		lo.Ternary(status == http.StatusServiceUnavailable, rp.pages.Error503, rp.pages.Error404)(w)
	}

	params, err := parseArchiveRequest(r)
	if err != nil {
		l.Warn("rejecting archive request", "err", err)
		fail(http.StatusNotFound)
		return
	}

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		fail(http.StatusNotFound)
		return
	}

	name := gitutil.RepoName(f.Slug())
	params.Prefix = params.Prefix.OrDefault(name, params.Rev)

	// build the xrpc url
	xrpcURL := fmt.Sprintf("%s/xrpc/%s?%s",
		rp.config.KnotMirror.Url, tangled.GitTempGetArchiveNSID, params.Query(f.RepoDid).Encode())

	// make the get request
	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, xrpcURL, nil)
	if err != nil {
		l.Error("failed to build XRPC repo.archive request", "err", err)
		fail(http.StatusServiceUnavailable)
		return
	}
	resp, err := rp.archiveClient.Do(req)
	if err != nil {
		l.Error("failed to call XRPC repo.archive", "err", err)
		fail(http.StatusServiceUnavailable)
		return
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		l.Error("XRPC repo.archive failed", "status", resp.StatusCode, "ref", params.Rev)
		overloaded := resp.StatusCode >= http.StatusInternalServerError || resp.StatusCode == http.StatusTooManyRequests
		fail(lo.Ternary(overloaded, http.StatusServiceUnavailable, http.StatusNotFound))
		return
	}

	params.SetHeaders(w.Header(), name)

	if resolvedRev, err := gitutil.ParseImmutableLink(resp.Header.Get("Link")); err == nil {
		w.Header().Set("Link", gitutil.ImmutableLink(rp.immutableArchiveURL(f, params.WithRev(resolvedRev))))
	}

	// stream the archive data directly
	if _, err := io.Copy(w, resp.Body); err != nil {
		l.Error("failed to write response", "err", err)
	}
}

func parseArchiveRequest(r *http.Request) (gitutil.ArchiveParams, error) {
	ref := chi.URLParam(r, "*")
	if unescaped, err := url.PathUnescape(ref); err == nil && r.URL.RawPath != "" {
		ref = unescaped
	}

	suffix, found := lo.Find(gitutil.ArchiveFormats, func(f gitutil.ArchiveFormat) bool {
		return strings.HasSuffix(ref, "."+f.String())
	})
	if found {
		ref = strings.TrimSuffix(ref, "."+suffix.String())
	}

	rev, err := gitutil.ParseRev(ref)
	if err != nil {
		return gitutil.ArchiveParams{}, err
	}

	query := r.URL.Query()
	query.Del("ref")
	query.Set("format", archiveFormat(query.Get("format"), suffix, r.UserAgent()).String())
	params, err := gitutil.ParseArchiveParams(query)
	if err != nil {
		return gitutil.ArchiveParams{}, err
	}
	return params.WithRev(rev), nil
}

func archiveFormat(requested string, suffix gitutil.ArchiveFormat, userAgent string) gitutil.ArchiveFormat {
	if format, err := gitutil.ParseArchiveFormat(requested); err == nil {
		return format
	}
	if suffix != "" {
		return suffix
	}
	ua := strings.ToLower(userAgent)
	windows := lo.SomeBy([]string{"windows", "win64", "win32"}, func(s string) bool { return strings.Contains(ua, s) })
	return lo.Ternary(windows, gitutil.ArchiveZip, gitutil.ArchiveTarGz)
}

func (rp *Repo) immutableArchiveURL(f *models.Repo, params gitutil.ArchiveParams) string {
	return fmt.Sprintf("%s/%s/archive/%s.%s?%s",
		rp.config.Core.BaseUrl(), f.RepoIdentifier(),
		url.PathEscape(params.Rev.String()), params.Format,
		url.Values{"prefix": {params.Prefix.String()}}.Encode())
}
