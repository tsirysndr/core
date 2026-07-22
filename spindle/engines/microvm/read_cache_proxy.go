package microvm

import (
	"context"
	"crypto/rand"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/mdlayher/vsock"
)

const (
	readCacheProxyPortMin = 20000
	readCacheProxyPortMax = 60000
)

type ReadCacheProxy struct {
	port uint32

	ln     *vsock.Listener
	server *http.Server
}

func StartReadCacheProxy(ctx context.Context, cid uint32, upstreams []CacheUpstream, logger *slog.Logger) (*ReadCacheProxy, error) {
	if logger == nil {
		logger = slog.Default()
	}
	logger = logger.With("where", "read_cache", "cid", cid)

	if len(upstreams) == 0 {
		return nil, nil
	}

	ln, port, err := listenRandomVsockPort(ctx)
	if err != nil {
		return nil, err
	}

	proxy := &ReadCacheProxy{
		port: port,
		ln:   ln,
	}
	proxy.server = &http.Server{
		Handler:           cacheProxyHandler(upstreams, logger),
		Protocols:         cacheProxyProtocols(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	filtered := &cidFilteredVsockListener{
		Listener: ln,
		cid:      cid,
		logger:   logger,
	}
	go func() {
		if err := proxy.server.Serve(filtered); err != nil && !errors.Is(err, http.ErrServerClosed) && !errors.Is(err, net.ErrClosed) {
			logger.Warn("proxy stopped", "cid", cid, "port", port, "error", err)
		}
	}()

	logger.Info("started proxy", "cid", cid, "port", port, "upstreams", len(upstreams))
	return proxy, nil
}

func (p *ReadCacheProxy) Port() uint32 {
	if p == nil {
		return 0
	}
	return p.port
}

func (p *ReadCacheProxy) Close() error {
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

type cidFilteredVsockListener struct {
	*vsock.Listener
	cid    uint32
	logger *slog.Logger
}

func (l *cidFilteredVsockListener) Accept() (net.Conn, error) {
	for {
		conn, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}

		addr, ok := conn.RemoteAddr().(*vsock.Addr)
		if ok && addr.ContextID == l.cid {
			return conn, nil
		}

		l.logger.Warn("dropping proxy connection from unexpected cid", "remote", conn.RemoteAddr(), "expectedCID", l.cid)
		_ = conn.Close()
	}
}

func parseCacheUpstreams(raw []string) ([]*url.URL, error) {
	upstreams := make([]*url.URL, 0, len(raw))
	seen := make(map[string]struct{}, len(raw))
	for _, value := range raw {
		value = strings.TrimSpace(value)
		if value == "" {
			continue
		}
		if _, ok := seen[value]; ok {
			continue
		}
		seen[value] = struct{}{}

		parsed, err := url.Parse(value)
		if err != nil {
			return nil, fmt.Errorf("parse URL %q: %w", value, err)
		}
		if parsed.Scheme != "http" && parsed.Scheme != "https" {
			return nil, fmt.Errorf("URL %q uses unsupported scheme %q", value, parsed.Scheme)
		}
		if parsed.Host == "" {
			return nil, fmt.Errorf("URL %q is missing host", value)
		}
		upstreams = append(upstreams, parsed)
	}
	return upstreams, nil
}

type CacheUpstream struct {
	url *url.URL
	// guarded upstreams come from the workflow file
	// requests to them are refused for special-purpose address ranges
	guarded bool
}

func BuildCacheUpstreams(rawTrusted, rawGuarded []string) ([]CacheUpstream, error) {
	trusted, err := parseCacheUpstreams(rawTrusted)
	if err != nil {
		return nil, err
	}
	guarded, err := parseCacheUpstreams(rawGuarded)
	if err != nil {
		return nil, err
	}
	return mergeCacheUpstreams(trusted, guarded), nil
}

func mergeCacheUpstreams(trusted, guarded []*url.URL) []CacheUpstream {
	merged := make([]CacheUpstream, 0, len(trusted)+len(guarded))
	seen := make(map[string]struct{}, len(trusted)+len(guarded))
	for _, u := range trusted {
		if _, ok := seen[u.String()]; ok {
			continue
		}
		seen[u.String()] = struct{}{}
		merged = append(merged, CacheUpstream{url: u})
	}
	for _, u := range guarded {
		if _, ok := seen[u.String()]; ok {
			continue
		}
		seen[u.String()] = struct{}{}
		merged = append(merged, CacheUpstream{url: u, guarded: true})
	}
	return merged
}

func listenRandomVsockPort(ctx context.Context) (*vsock.Listener, uint32, error) {
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
	return nil, 0, fmt.Errorf("listen on random vsock port: %w", lastErr)
}

func randomVsockPort() (uint32, error) {
	var data [4]byte
	if _, err := rand.Read(data[:]); err != nil {
		return 0, fmt.Errorf("allocate read vsock port: %w", err)
	}
	span := uint32(readCacheProxyPortMax - readCacheProxyPortMin)
	return readCacheProxyPortMin + binary.BigEndian.Uint32(data[:])%span, nil
}

var proxyTransport = &http.Transport{
	Proxy:                 http.ProxyFromEnvironment,
	ForceAttemptHTTP2:     true,
	MaxIdleConns:          100,
	IdleConnTimeout:       90 * time.Second,
	TLSHandshakeTimeout:   10 * time.Second,
	ExpectContinueTimeout: 1 * time.Second,
}

// for guarded upstreams, this will refuse requests made to blocked addresses
var guardedProxyTransport = &http.Transport{
	DialContext: (&net.Dialer{
		Timeout:   30 * time.Second,
		KeepAlive: 30 * time.Second,
		Control:   refuseSpecialPurposeAddrs,
	}).DialContext,
	ForceAttemptHTTP2:     true,
	MaxIdleConns:          100,
	IdleConnTimeout:       90 * time.Second,
	TLSHandshakeTimeout:   10 * time.Second,
	ExpectContinueTimeout: 1 * time.Second,
}

// this should run after dns resolution, so it should cover any rebinding tricks
func refuseSpecialPurposeAddrs(network, address string, _ syscall.RawConn) error {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return fmt.Errorf("split dial address %q: %w", address, err)
	}
	ip := net.ParseIP(host)
	if ip == nil {
		return fmt.Errorf("refusing to dial non-IP address %q", host)
	}
	bits := 128
	if ip4 := ip.To4(); ip4 != nil {
		ip = ip4
		bits = 32
	}
	for _, ipnet := range blockedNamespaceNets {
		_, blockedBits := ipnet.Mask.Size()
		if blockedBits != bits {
			continue
		}
		if ipnet.Contains(ip) {
			return fmt.Errorf("refusing to dial %s: %s is blocked for workflow caches", ip, ipnet)
		}
	}
	return nil
}

// the proxy is the cache as far as the guest is concerned, so we answer
// /nix-cache-info ourselves instead of racing the upstreams for it. merging
// those also doesn't make any sense (none of the options make sense for
// merging)
const nixCacheInfo = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n"

func cacheProxyHandler(upstreams []CacheUpstream, logger *slog.Logger) http.Handler {
	proxy := &httputil.ReverseProxy{
		// nothing to do here: the racing transport builds the full URL per
		// upstream, it just needs the guest's path/query left intact
		Rewrite:  func(*httputil.ProxyRequest) {},
		ErrorLog: slog.NewLogLogger(logger.Handler(), slog.LevelError),
		Transport: &parallelRacingTransport{
			upstreams:         upstreams,
			underlying:        proxyTransport,
			guardedUnderlying: guardedProxyTransport,
			logger:            logger,
		},
	}

	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/nix-cache-info" {
			w.Header().Set("Content-Type", "text/x-nix-cache-info")
			_, _ = io.WriteString(w, nixCacheInfo)
			return
		}
		proxy.ServeHTTP(w, r)
	})
}

func cacheProxyProtocols() *http.Protocols {
	protocols := new(http.Protocols)
	protocols.SetHTTP1(true)
	protocols.SetUnencryptedHTTP2(true)
	return protocols
}

func mergeQuery(base, extra string) string {
	switch {
	case base == "":
		return extra
	case extra == "":
		return base
	default:
		return base + "&" + extra
	}
}

type parallelRacingTransport struct {
	upstreams         []CacheUpstream
	underlying        http.RoundTripper
	guardedUnderlying http.RoundTripper
	logger            *slog.Logger
}

func (t *parallelRacingTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	type result struct {
		resp  *http.Response
		err   error
		is404 bool
		idx   int
	}

	resCh := make(chan result, len(t.upstreams))
	cancels := make([]context.CancelFunc, len(t.upstreams))
	var wg sync.WaitGroup

	for i, upstream := range t.upstreams {
		wg.Add(1)
		ctx, cancel := context.WithCancel(req.Context())
		cancels[i] = cancel

		go func(idx int, target CacheUpstream, uCtx context.Context) {
			defer wg.Done()

			raceReq := req.Clone(uCtx)
			// rewrite to the target, joining the upstream's base path/query
			// with what the guest asked for
			raceReq.URL.Scheme = target.url.Scheme
			raceReq.URL.Host = target.url.Host
			raceReq.URL.Path = strings.TrimSuffix(target.url.Path, "/") + req.URL.Path
			raceReq.URL.RawQuery = mergeQuery(target.url.RawQuery, req.URL.RawQuery)
			// Host wins over URL.Host for the outgoing Host header, and the
			// reverse proxy preserves the guest's (127.0.0.1:<port>), which
			// host-routed upstreams like fastly reject with a 421
			raceReq.Host = target.url.Host
			// the transport doesn't turn URL userinfo into basic auth, only
			// http.Client does, so do it ourselves
			if user := target.url.User; user != nil {
				password, _ := user.Password()
				raceReq.SetBasicAuth(user.Username(), password)
			}

			rt := t.underlying
			if target.guarded {
				rt = t.guardedUnderlying
			}
			resp, err := rt.RoundTrip(raceReq)
			if err != nil {
				resCh <- result{err: err, idx: idx}
				return
			}
			if resp.StatusCode == http.StatusNotFound {
				_ = resp.Body.Close() // don't care about the body of a 404
				resCh <- result{is404: true, idx: idx}
				return
			}
			if resp.StatusCode >= 400 {
				// an erroring upstream must not win over a healthy one
				_ = resp.Body.Close()
				resCh <- result{err: fmt.Errorf("upstream returned status %d", resp.StatusCode), idx: idx}
				return
			}
			// yay, ok
			resCh <- result{resp: resp, idx: idx}
		}(i, upstream, ctx)
	}

	go func() {
		wg.Wait()
		close(resCh)
	}()

	var total404s int
	for res := range resCh {
		if res.is404 {
			total404s++
			if total404s == len(t.upstreams) {
				for _, cancel := range cancels {
					cancel()
				}
				return &http.Response{
					StatusCode: http.StatusNotFound,
					Body:       io.NopCloser(strings.NewReader("404 nix path not found")),
					Header:     make(http.Header),
					Request:    req,
				}, nil
			}
			continue
		}

		if res.err != nil {
			if !errors.Is(res.err, context.Canceled) {
				t.logger.Warn("upstream failed",
					"path", req.URL.Path,
					"error", res.err,
				)
			}
			continue
		}

		// cancel other requests
		for i, cancel := range cancels {
			if i != res.idx {
				cancel()
			}
		}
		return res.resp, nil
	}

	for _, cancel := range cancels {
		cancel()
	}
	return nil, errors.New("all upstreams failed or timed out")
}
