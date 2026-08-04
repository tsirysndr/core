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

	request, err := parseArchiveRequest(r)
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

	served := request.params.Serve(gitutil.RepoName(f.Slug()))

	// build the xrpc url
	xrpcURL := fmt.Sprintf("%s/xrpc/%s?%s",
		rp.config.KnotMirror.Url, tangled.GitTempGetArchiveNSID, served.Query(f.RepoDid).Encode())

	// make the get request
	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, xrpcURL, nil)
	if err != nil {
		l.Error("failed to build XRPC repo.archive request", "err", err)
		fail(http.StatusServiceUnavailable)
		return
	}
	gitutil.ForwardHeaders(req.Header, r.Header, "If-None-Match")
	resp, err := rp.archiveClient.Do(req)
	if err != nil {
		l.Error("failed to call XRPC repo.archive", "err", err)
		fail(http.StatusServiceUnavailable)
		return
	}
	defer resp.Body.Close()

	revalidated := resp.StatusCode == http.StatusNotModified
	if resp.StatusCode != http.StatusOK && !revalidated {
		l.Error("XRPC repo.archive failed", "status", resp.StatusCode, "ref", request.params.Rev)
		overloaded := resp.StatusCode >= http.StatusInternalServerError || resp.StatusCode == http.StatusTooManyRequests
		fail(lo.Ternary(overloaded, http.StatusServiceUnavailable, http.StatusNotFound))
		return
	}

	served.SetHeaders(w.Header())
	gitutil.ForwardHeaders(w.Header(), resp.Header, "Etag")

	resolvedRev, _ := gitutil.ParseImmutableLink(resp.Header.Get("Link"))
	if resolvedRev != "" {
		w.Header().Set("Link", gitutil.ImmutableLink(rp.immutableArchiveURL(f, served.WithRev(resolvedRev))))
	}
	setArchiveCache(w.Header(), request, resolvedRev)

	if revalidated {
		w.WriteHeader(http.StatusNotModified)
		return
	}
	gitutil.ForwardHeaders(w.Header(), resp.Header, "Content-Length")

	// stream the archive data directly
	body := gitutil.NewResponseBody(w)
	if _, err := io.Copy(body, resp.Body); err != nil {
		l.Error("failed to write response", "err", err)
		body.Fail()
	}
}

type archiveRequest struct {
	params  gitutil.ArchiveParams
	guessed bool
}

const immutableArchiveCache = "public, max-age=31536000"

func setArchiveCache(h http.Header, request archiveRequest, resolved gitutil.Rev) {
	addressed := resolved.IsObjectID() && request.params.Rev == resolved &&
		request.params.Prefix != "" && !request.guessed
	h.Set("Cache-Control", lo.Ternary(addressed, immutableArchiveCache, "no-cache"))
	if request.guessed {
		h.Add("Vary", "User-Agent")
	}
}

func parseArchiveRequest(r *http.Request) (archiveRequest, error) {
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
		return archiveRequest{}, err
	}

	query := r.URL.Query()
	query.Del("ref")
	format, guessed := archiveFormat(query.Get("format"), suffix, r.UserAgent())
	query.Set("format", format.String())
	params, err := gitutil.ParseArchiveParams(query)
	if err != nil {
		return archiveRequest{}, err
	}
	return archiveRequest{params: params.WithRev(rev), guessed: guessed}, nil
}

func archiveFormat(requested string, suffix gitutil.ArchiveFormat, userAgent string) (gitutil.ArchiveFormat, bool) {
	if format, err := gitutil.ParseArchiveFormat(requested); err == nil {
		return format, false
	}
	if suffix != "" {
		return suffix, false
	}
	ua := strings.ToLower(userAgent)
	windows := lo.SomeBy([]string{"windows", "win64", "win32"}, func(s string) bool { return strings.Contains(ua, s) })
	return lo.Ternary(windows, gitutil.ArchiveZip, gitutil.ArchiveTarGz), true
}

func (rp *Repo) immutableArchiveURL(f *models.Repo, archive gitutil.ServedArchive) string {
	return fmt.Sprintf("%s/%s/archive/%s",
		rp.config.Core.BaseUrl(), f.RepoIdentifier(), archive.SuffixURL())
}
