package microvm

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
	"time"

	"github.com/mdlayher/vsock"
)

type UploadCacheProxy struct {
	port uint32

	ln     *vsock.Listener
	server *http.Server
}

func StartUploadCacheProxy(ctx context.Context, cid uint32, uploadURL string, readUpstreams []CacheUpstream, logger *slog.Logger) (*UploadCacheProxy, error) {
	if strings.TrimSpace(uploadURL) == "" {
		return nil, nil
	}

	if logger == nil {
		logger = slog.Default()
	}
	logger = logger.With("where", "upload_cache_proxy", "cid", cid, "uploadURL", uploadURL)

	target, err := url.Parse(uploadURL)
	if err != nil {
		return nil, fmt.Errorf("parse upload URL %q: %w", uploadURL, err)
	}
	if target.Scheme != "http" && target.Scheme != "https" {
		return nil, fmt.Errorf("upload URL %q uses unsupported scheme %q (must be http or https)", uploadURL, target.Scheme)
	}
	if target.Host == "" {
		return nil, fmt.Errorf("upload URL %q is missing host", uploadURL)
	}

	ln, port, err := listenRandomVsockUploadPort(ctx)
	if err != nil {
		return nil, fmt.Errorf("listen for cache upload proxy: %w", err)
	}

	proxy := &UploadCacheProxy{
		port: port,
		ln:   ln,
	}
	proxy.server = &http.Server{
		Handler:           uploadProxyHandler(target, readUpstreams, logger),
		Protocols:         cacheProxyProtocols(),
		ReadHeaderTimeout: 30 * time.Second,
	}

	filtered := &cidFilteredVsockListener{
		Listener: ln,
		cid:      cid,
		logger:   logger,
	}
	go func() {
		if err := proxy.server.Serve(filtered); err != nil && !errors.Is(err, http.ErrServerClosed) && !errors.Is(err, net.ErrClosed) {
			logger.Warn("upload cache proxy stopped", "port", port, "error", err)
		}
	}()

	logger.Info("started upload cache proxy", "port", port, "target", uploadURL, "readUpstreams", len(readUpstreams))
	return proxy, nil
}

func (p *UploadCacheProxy) Port() uint32 {
	if p == nil {
		return 0
	}
	return p.port
}

func (p *UploadCacheProxy) Close() error {
	if p == nil {
		return nil
	}

	var closeErr error
	if p.server != nil {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		closeErr = errors.Join(closeErr, p.server.Shutdown(ctx))
		cancel()
		p.server = nil
	}
	if p.ln != nil {
		closeErr = errors.Join(closeErr, p.ln.Close())
		p.ln = nil
	}
	return closeErr
}

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
	exists := &parallelRacingTransport{
		upstreams:         narinfoUpstreams,
		underlying:        proxyTransport,
		guardedUnderlying: guardedProxyTransport,
		logger:            logger,
	}

	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if isNarinfoExistenceCheck(r) {
			serveNarinfoExistence(w, r, exists, logger)
			return
		}
		rp.ServeHTTP(w, r)
	})
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

func listenRandomVsockUploadPort(ctx context.Context) (*vsock.Listener, uint32, error) {
	var lastErr error
	for range 32 {
		port, err := randomVsockPort()
		if err != nil {
			return nil, 0, err
		}
		ln, err := vsock.Listen(port, nil)
		if err == nil {
			return ln, port, nil
		}
		lastErr = err

		select {
		case <-ctx.Done():
			return nil, 0, ctx.Err()
		default:
		}
	}
	return nil, 0, fmt.Errorf("listen on random vsock upload port: %w", lastErr)
}
