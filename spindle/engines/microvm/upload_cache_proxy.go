package microvm

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/mdlayher/vsock"
)

type UploadCacheBackend interface {
	http.Handler
	Close() error
}

type UploadCacheProxy struct {
	port uint32

	ln      *vsock.Listener
	server  *http.Server
	backend UploadCacheBackend
}

func StartUploadCacheProxy(ctx context.Context, cid uint32, uploadURL string, readUpstreams []CacheUpstream, stagingDir string, logger *slog.Logger) (*UploadCacheProxy, error) {
	if strings.TrimSpace(uploadURL) == "" {
		return nil, nil
	}

	if logger == nil {
		logger = slog.Default()
	}
	logger = logger.With("where", "upload_cache_proxy", "cid", cid, "uploadURL", uploadURL)

	backend, err := newUploadCacheBackend(uploadURL, readUpstreams, stagingDir, logger)
	if err != nil {
		return nil, err
	}

	ln, port, err := listenRandomVsockUploadPort(ctx)
	if err != nil {
		return nil, fmt.Errorf("listen for cache upload proxy: %w", err)
	}

	proxy := &UploadCacheProxy{
		port:    port,
		ln:      ln,
		backend: backend,
	}
	proxy.server = &http.Server{
		Handler:           backend,
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

func newUploadCacheBackend(uploadURL string, readUpstreams []CacheUpstream, stagingDir string, logger *slog.Logger) (UploadCacheBackend, error) {
	if strings.TrimSpace(uploadURL) == "" {
		return nil, nil
	}

	target, err := url.Parse(uploadURL)
	if err != nil {
		return nil, fmt.Errorf("parse upload URL %q: %w", uploadURL, err)
	}

	switch target.Scheme {
	case "http", "https":
		if target.Host == "" {
			return nil, fmt.Errorf("upload URL %q is missing host", uploadURL)
		}
		return newHTTPUploadProxyBackend(target, readUpstreams, logger), nil

	case "ssh", "ssh-ng":
		return newNixStoreUploadBackend(target.String(), stagingDir, readUpstreams, logger, nil)

	case "":
		switch uploadURL {
		case "daemon", "local":
			return newNixStoreUploadBackend(uploadURL, stagingDir, readUpstreams, logger, nil)
		default:
			return nil, fmt.Errorf("unsupported upload URL %q", uploadURL)
		}

	default:
		return nil, fmt.Errorf("upload URL %q uses unsupported scheme %q", uploadURL, target.Scheme)
	}
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
	if p.backend != nil {
		closeErr = errors.Join(closeErr, p.backend.Close())
	}
	return closeErr
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
