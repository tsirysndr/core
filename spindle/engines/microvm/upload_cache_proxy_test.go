//go:build linux

package microvm

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
)

func TestUploadProxyRewritesHostAndAuth(t *testing.T) {
	var upstreamHost string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.Host != upstreamHost {
			t.Errorf("host: got %q, want %q", req.Host, upstreamHost)
		}
		if req.URL.Path != "/sub/abc.narinfo" {
			t.Errorf("path: got %q, want /sub/abc.narinfo", req.URL.Path)
		}
		if user, pass, ok := req.BasicAuth(); !ok || user != "dawn" || pass != "woof" {
			t.Errorf("basic auth: got %q/%q/%v, want dawn/hunter2/true", user, pass, ok)
		}
		_, _ = io.WriteString(w, "ok")
	}))
	defer upstream.Close()
	upstreamHost = strings.TrimPrefix(upstream.URL, "http://")

	target, err := url.Parse("http://dawn:woof@" + upstreamHost + "/sub/")
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodPut, "http://127.0.0.1:10501/abc.narinfo", strings.NewReader("narinfo"))
	req.Host = "127.0.0.1:10501"
	rec := httptest.NewRecorder()
	uploadProxyHandler(target, nil, slog.Default()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}
}

func mustParseURL(t *testing.T, raw string) *url.URL {
	t.Helper()
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatalf("parse %q: %v", raw, err)
	}
	return u
}

func TestUploadProxySkipsNarinfoAvailableUpstream(t *testing.T) {
	var uploadHits int
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		uploadHits++
		w.WriteHeader(http.StatusNotFound)
	}))
	defer target.Close()

	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.URL.Path != "/abc.narinfo" {
			t.Errorf("upstream path: got %q, want /abc.narinfo", req.URL.Path)
		}
		_, _ = io.WriteString(w, "StorePath: /nix/store/abc\n")
	}))
	defer upstream.Close()

	handler := uploadProxyHandler(
		mustParseURL(t, target.URL),
		[]CacheUpstream{{url: mustParseURL(t, upstream.URL)}},
		slog.Default(),
	)

	req := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:10501/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200 (so nix treats the path as present and skips upload)", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "StorePath: /nix/store/abc") {
		t.Fatalf("body: got %q, want the upstream narinfo body", rec.Body.String())
	}
}

func TestUploadProxyUploadsNarinfoNobodyHas(t *testing.T) {
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		w.WriteHeader(http.StatusNotFound)
	}))
	defer target.Close()
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		w.WriteHeader(http.StatusNotFound)
	}))
	defer upstream.Close()

	handler := uploadProxyHandler(
		mustParseURL(t, target.URL),
		[]CacheUpstream{{url: mustParseURL(t, upstream.URL)}},
		slog.Default(),
	)

	req := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:10501/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusNotFound {
		t.Fatalf("status: got %d, want 404 (so nix uploads the path)", rec.Code)
	}
}

func TestUploadProxySkipsNarinfoAlreadyOnTarget(t *testing.T) {
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		_, _ = io.WriteString(w, "StorePath: /nix/store/abc\n")
	}))
	defer target.Close()

	handler := uploadProxyHandler(mustParseURL(t, target.URL), nil, slog.Default())

	req := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:10501/abc.narinfo", nil)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status: got %d, want 200", rec.Code)
	}
}
