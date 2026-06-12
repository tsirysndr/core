package microvm

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestCacheProxyFallsBackOnNotFound(t *testing.T) {
	first := httptest.NewServer(http.NotFoundHandler())
	defer first.Close()
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.URL.Path != "/abc.narinfo" {
			t.Fatalf("path: got %q, want /abc.narinfo", req.URL.Path)
		}
		_, _ = io.WriteString(w, "ok")
	}))
	defer second.Close()

	upstreams, err := parseCacheUpstreams([]string{first.URL, second.URL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://guest/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(upstreams, nil), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
	if got := rec.Body.String(); got != "ok" {
		t.Fatalf("body: got %q, want ok", got)
	}
}

func TestCacheProxyServesNixCacheInfoItself(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		t.Errorf("upstream should not be hit, got request for %q", req.URL.Path)
	}))
	defer upstream.Close()

	upstreams, err := parseCacheUpstreams([]string{upstream.URL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://guest/nix-cache-info", nil)
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(upstreams, nil), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
	if got := rec.Body.String(); got != nixCacheInfo {
		t.Fatalf("body: got %q, want %q", got, nixCacheInfo)
	}
}

func TestCacheProxyErrorStatusDoesNotWinRace(t *testing.T) {
	erroring := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		http.Error(w, "misdirected", http.StatusMisdirectedRequest)
	}))
	defer erroring.Close()
	healthy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		time.Sleep(50 * time.Millisecond) // lose the race to the erroring upstream
		_, _ = io.WriteString(w, "ok")
	}))
	defer healthy.Close()

	upstreams, err := parseCacheUpstreams([]string{erroring.URL, healthy.URL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://guest/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(upstreams, nil), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
	if got := rec.Body.String(); got != "ok" {
		t.Fatalf("body: got %q, want ok", got)
	}
}

func TestCacheProxyJoinsSubpathQueryAndAuth(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.URL.Path != "/sub/cache/abc.narinfo" {
			t.Errorf("path: got %q, want /sub/cache/abc.narinfo", req.URL.Path)
		}
		if got := req.URL.Query().Get("token"); got != "s3cret" {
			t.Errorf("token: got %q, want s3cret", got)
		}
		if user, pass, ok := req.BasicAuth(); !ok || user != "dawn" || pass != "woof" {
			t.Errorf("basic auth: got %q/%q/%v, want dawn/woof/true", user, pass, ok)
		}
		_, _ = io.WriteString(w, "ok")
	}))
	defer upstream.Close()

	upstreamURL := "http://dawn:woof@" + strings.TrimPrefix(upstream.URL, "http://") + "/sub/cache/?token=s3cret"
	upstreams, err := parseCacheUpstreams([]string{upstreamURL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://guest/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(upstreams, nil), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
	if got := rec.Body.String(); got != "ok" {
		t.Fatalf("body: got %q, want ok", got)
	}
}

func TestCacheProxyGuardedUpstreamCannotReachBlockedRanges(t *testing.T) {
	// httptest listens on 127.0.0.1, which is in the blocked ranges; reaching
	// it would mean a workflow-defined cache can hit the host's loopback
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		t.Errorf("guarded upstream on loopback should not be reachable, got request for %q", req.URL.Path)
	}))
	defer upstream.Close()

	upstreams, err := parseCacheUpstreams([]string{upstream.URL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://guest/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(nil, upstreams), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusBadGateway {
		t.Fatalf("status: got %d, want 502; body=%q", rec.Code, rec.Body.String())
	}
}

func TestCacheProxyRewritesHostHeader(t *testing.T) {
	var upstreamHost string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.Host != upstreamHost {
			t.Errorf("host: got %q, want %q", req.Host, upstreamHost)
		}
		_, _ = io.WriteString(w, "ok")
	}))
	defer upstream.Close()
	upstreamHost = strings.TrimPrefix(upstream.URL, "http://")

	upstreams, err := parseCacheUpstreams([]string{upstream.URL})
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:10500/abc.narinfo", nil)
	req.Host = "127.0.0.1:10500"
	rec := httptest.NewRecorder()
	cacheProxyHandler(mergeCacheUpstreams(upstreams, nil), slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
}
