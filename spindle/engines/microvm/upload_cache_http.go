package microvm

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
)

// httpUploadBackend reverse-proxies guest binary-cache upload traffic to an
// http(s) upload cache such as ncps.
type httpUploadBackend struct {
	handler http.Handler
}

func newHTTPUploadProxyBackend(target *url.URL, readUpstreams []CacheUpstream, logger *slog.Logger) *httpUploadBackend {
	return &httpUploadBackend{handler: uploadProxyHandler(target, readUpstreams, logger)}
}

func (b *httpUploadBackend) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	b.handler.ServeHTTP(w, r)
}

func (b *httpUploadBackend) Close() error { return nil }

func uploadProxyHandler(target *url.URL, readUpstreams []CacheUpstream, logger *slog.Logger) http.Handler {
	rp := httputil.NewSingleHostReverseProxy(target)
	rp.ErrorLog = slog.NewLogLogger(logger.Handler(), slog.LevelError)

	origDirector := rp.Director
	rp.Director = func(req *http.Request) {
		origDirector(req)
		// ensure host matches target
		req.Host = target.Host
		// the transport doesn't turn URL userinfo into basic auth, only
		// http.Client does, so do it ourselves
		if user := target.User; user != nil {
			password, _ := user.Password()
			req.SetBasicAuth(user.Username(), password)
		}
	}

	// before uploading, nix copy asks the destination whether it already has each
	// path by GET/HEAD-ing <hash>.narinfo and skips the ones it does. we answer
	// that check across the upload target *and* the read caches: if any of them
	// already serves the path there is no point uploading it (the guest would
	// just substitute it from there anyway).
	narinfoUpstreams := append([]CacheUpstream{{url: target}}, readUpstreams...)
	exists := newNarinfoExistenceTransport(narinfoUpstreams, logger)

	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if isNarinfoExistenceCheck(r) {
			serveNarinfoExistence(w, r, exists, logger)
			return
		}
		rp.ServeHTTP(w, r)
	})
}

func newNarinfoExistenceTransport(upstreams []CacheUpstream, logger *slog.Logger) http.RoundTripper {
	return &parallelRacingTransport{
		upstreams:         upstreams,
		underlying:        proxyTransport,
		guardedUnderlying: guardedProxyTransport,
		logger:            logger,
	}
}

func isNarinfoExistenceCheck(r *http.Request) bool {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		return false
	}
	return strings.HasSuffix(r.URL.Path, ".narinfo")
}

func serveNarinfoExistence(w http.ResponseWriter, r *http.Request, exists http.RoundTripper, logger *slog.Logger) {
	probe := r.Clone(r.Context())
	probe.RequestURI = ""

	resp, err := exists.RoundTrip(probe)
	if err != nil {
		logger.Warn("upload proxy narinfo check failed, treating as not present", "path", r.URL.Path, "error", err)
		w.WriteHeader(http.StatusNotFound)
		return
	}
	defer resp.Body.Close()

	for key, values := range resp.Header {
		for _, value := range values {
			w.Header().Add(key, value)
		}
	}
	w.WriteHeader(resp.StatusCode)
	if _, err := io.Copy(w, resp.Body); err != nil && !errors.Is(err, context.Canceled) {
		logger.Warn("upload proxy narinfo copy failed", "path", r.URL.Path, "error", err)
	}
}
