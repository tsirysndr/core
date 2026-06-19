package microvm

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
)

const (
	testStoreHash = "0123456789abcdfghijklmnpqrsvwxyz"
	testStorePath = "/nix/store/" + testStoreHash + "-abc-output"
)

func TestUploadCacheBackendSchemeDispatch(t *testing.T) {
	staging := t.TempDir()
	logger := slog.Default()

	cases := []struct {
		uploadURL string
		wantErr   bool
		wantType  string
	}{
		{"https://cache.example/upload", false, "*microvm.httpUploadBackend"},
		{"http://cache.example/upload", false, "*microvm.httpUploadBackend"},
		{"ssh://cache-host", false, "*microvm.NixStoreUploadBackend"},
		{"ssh-ng://cache-host", false, "*microvm.NixStoreUploadBackend"},
		{"daemon", false, "*microvm.NixStoreUploadBackend"},
		{"local", false, "*microvm.NixStoreUploadBackend"},
		{"ftp://cache.example", true, ""},
		{"/some/path", true, ""},
	}

	for _, tc := range cases {
		t.Run(tc.uploadURL, func(t *testing.T) {
			backend, err := newUploadCacheBackend(tc.uploadURL, nil, staging, logger)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("expected error for %q", tc.uploadURL)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			got := fmt.Sprintf("%T", backend)
			if got != tc.wantType {
				t.Fatalf("backend type: got %s, want %s", got, tc.wantType)
			}
		})
	}
}

func TestUploadCacheBackendEmptyURL(t *testing.T) {
	backend, err := newUploadCacheBackend("", nil, t.TempDir(), slog.Default())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if backend != nil {
		t.Fatalf("expected nil backend for empty URL, got %T", backend)
	}
}

func newTestNixStoreBackend(t *testing.T, target string, runner CommandRunner) (*NixStoreUploadBackend, string) {
	t.Helper()
	staging := t.TempDir()
	if target == "" {
		target = "ssh-ng://cache-host"
	}
	b, err := newNixStoreUploadBackend(target, staging, nil, slog.Default(), runner)
	if err != nil {
		t.Fatalf("newNixStoreUploadBackend: %v", err)
	}
	return b, staging
}

func mustUploadNar(t *testing.T, b *NixStoreUploadBackend, name, body string) {
	t.Helper()
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/nar/"+name, strings.NewReader(body)))
	if rec.Code != http.StatusOK {
		t.Fatalf("upload nar %q: got %d, want 200; body=%q", name, rec.Code, rec.Body.String())
	}
}

func TestNixStoreBackendNixCacheInfo(t *testing.T) {
	b, _ := newTestNixStoreBackend(t, "", nil)

	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/nix-cache-info", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET status: got %d, want 200", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "StoreDir: /nix/store") {
		t.Fatalf("cache info missing StoreDir: %q", rec.Body.String())
	}

	rec = httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodHead, "/nix-cache-info", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("HEAD status: got %d, want 200", rec.Code)
	}
	if rec.Body.Len() != 0 {
		t.Fatalf("HEAD body should be empty, got %q", rec.Body.String())
	}

	rec = httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/nix-cache-info", strings.NewReader("ignored")))
	if rec.Code != http.StatusOK {
		t.Fatalf("PUT /nix-cache-info status: got %d, want 200", rec.Code)
	}
}

func TestNixStoreBackendRejectsTraversalNar(t *testing.T) {
	b, staging := newTestNixStoreBackend(t, "", nil)

	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/nar/../../evil", strings.NewReader("bad")))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("traversal nar status: got %d, want 400", rec.Code)
	}

	if _, err := os.Stat(filepath.Join(filepath.Dir(staging), "evil")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("traversal nar escaped staging dir: %v", err)
	}
}

func TestNixStoreBackendRejectsOversizedNarUpload(t *testing.T) {
	b, staging := newTestNixStoreBackend(t, "", nil)
	b.maxNarUploadSize = 3

	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/nar/foo.nar", strings.NewReader("four")))
	if rec.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("oversized nar status: got %d, want 413; body=%q", rec.Code, rec.Body.String())
	}

	if _, err := os.Stat(filepath.Join(staging, "nar", "foo.nar")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("oversized nar should not have been staged: %v", err)
	}
}

func TestNixStoreBackendNarinfoRequiresExistingNar(t *testing.T) {
	b, _ := newTestNixStoreBackend(t, "", nil)

	narinfo := "StorePath: " + testStorePath + "\nURL: nar/abc.nar.zst\nNarHash: sha256:abc\nNarSize: 123\n"
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/"+testStoreHash+".narinfo", strings.NewReader(narinfo)))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("narinfo before nar status: got %d, want 400; body=%q", rec.Code, rec.Body.String())
	}
}

type fakeRunner struct {
	mu      sync.Mutex
	calls   [][]string
	nextErr error
}

func (f *fakeRunner) Run(ctx context.Context, name string, args ...string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	call := append([]string{name}, args...)
	f.calls = append(f.calls, call)
	return f.nextErr
}

func (f *fakeRunner) Calls() [][]string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return slices.Clone(f.calls)
}

func TestNixStoreBackendImportsNarinfoImmediately(t *testing.T) {
	runner := &fakeRunner{}
	b, staging := newTestNixStoreBackend(t, "ssh-ng://spindle-upload@cache-host", runner)

	mustUploadNar(t, b, "foo.nar.zst", "nar-body")

	narinfo := "StorePath: " + testStorePath + "\nURL: nar/foo.nar.zst\nNarHash: sha256:abc\nNarSize: 123\n"
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/"+testStoreHash+".narinfo", strings.NewReader(narinfo)))
	if rec.Code != http.StatusOK {
		t.Fatalf("PUT narinfo status: got %d, want 200; body=%q", rec.Code, rec.Body.String())
	}

	calls := runner.Calls()
	if len(calls) != 1 {
		t.Fatalf("expected 1 nix copy call, got %d", len(calls))
	}
	call := calls[0]
	wantFrom := (&url.URL{Scheme: "file", Path: staging}).String()
	want := []string{
		"nix",
		"copy",
		"--from", wantFrom,
		"--to", "ssh-ng://spindle-upload@cache-host",
		"--no-check-sigs",
		"--substitute-on-destination",
		testStorePath,
	}
	if !slices.Equal(call, want) {
		t.Fatalf("nix copy args:\n got: %v\nwant: %v", call, want)
	}

	data, err := os.ReadFile(filepath.Join(staging, testStoreHash+".narinfo"))
	if err != nil {
		t.Fatalf("staged narinfo missing: %v", err)
	}
	if string(data) != narinfo {
		t.Fatalf("staged narinfo contents: got %q, want %q", string(data), narinfo)
	}
}

func TestNixStoreBackendRemovesNarinfoOnImportFailure(t *testing.T) {
	runner := &fakeRunner{nextErr: errors.New("nix copy failed")}
	b, staging := newTestNixStoreBackend(t, "ssh://cache-host", runner)

	mustUploadNar(t, b, "foo.nar.zst", "nar-body")

	narinfo := "StorePath: " + testStorePath + "\nURL: nar/foo.nar.zst\nNarHash: sha256:abc\nNarSize: 123\n"
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/"+testStoreHash+".narinfo", strings.NewReader(narinfo)))
	if rec.Code != http.StatusBadGateway {
		t.Fatalf("failed import status: got %d, want 502; body=%q", rec.Code, rec.Body.String())
	}

	if _, err := os.Stat(filepath.Join(staging, testStoreHash+".narinfo")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("narinfo should be removed after failed import: %v", err)
	}
}

func TestNixStoreBackendNarinfoReadUpstream(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/miss.narinfo" {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		_, _ = io.WriteString(w, "StorePath: /nix/store/upstream\nURL: nar/upstream.nar\nNarHash: sha256:up\nNarSize: 1\n")
	}))
	defer upstream.Close()

	upURL, err := url.Parse(upstream.URL)
	if err != nil {
		t.Fatal(err)
	}

	staging := t.TempDir()
	b, err := newNixStoreUploadBackend("ssh://cache-host", staging, []CacheUpstream{{url: upURL}}, slog.Default(), nil)
	if err != nil {
		t.Fatalf("newNixStoreUploadBackend: %v", err)
	}

	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/present.narinfo", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET upstream-present narinfo status: got %d, want 200", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "/nix/store/upstream") {
		t.Fatalf("unexpected upstream narinfo body: %q", rec.Body.String())
	}

	rec = httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/miss.narinfo", nil))
	if rec.Code != http.StatusNotFound {
		t.Fatalf("GET upstream-missing narinfo status: got %d, want 404", rec.Code)
	}

}

func TestNixStoreBackendRejectsInvalidLocalNarinfo(t *testing.T) {
	b, staging := newTestNixStoreBackend(t, "", nil)

	if err := os.WriteFile(filepath.Join(staging, testStoreHash+".narinfo"), []byte("not-a-narinfo\n"), 0o644); err != nil {
		t.Fatalf("write invalid staged narinfo: %v", err)
	}

	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/"+testStoreHash+".narinfo", nil))
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("invalid local narinfo status: got %d, want 500; body=%q", rec.Code, rec.Body.String())
	}
}

func TestNixStoreBackendNarinfoValidation(t *testing.T) {
	b, _ := newTestNixStoreBackend(t, "", nil)

	mustUploadNar(t, b, "x.nar", "x")

	cases := []struct {
		name    string
		body    string
		wantErr string
	}{
		{
			name:    "missing StorePath",
			body:    "URL: nar/x.nar\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "StorePath",
		},
		{
			name:    "bad StorePath",
			body:    "StorePath: /tmp/evil\nURL: nar/x.nar\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "invalid StorePath",
		},
		{
			name:    "malformed StorePath",
			body:    "StorePath: /nix/store/not-a-real-store-path\nURL: nar/x.nar\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "invalid StorePath",
		},
		{
			name:    "missing URL",
			body:    "StorePath: " + testStorePath + "\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "URL",
		},
		{
			name:    "absolute URL",
			body:    "StorePath: " + testStorePath + "\nURL: /etc/passwd\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "URL",
		},
		{
			name:    "traversal URL",
			body:    "StorePath: " + testStorePath + "\nURL: nar/../../etc/passwd\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "URL",
		},
		{
			name:    "non nar URL",
			body:    "StorePath: " + testStorePath + "\nURL: nix-cache-info\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "nar/",
		},
		{
			name:    "nested nar URL",
			body:    "StorePath: " + testStorePath + "\nURL: nar/dir/x.nar\nNarHash: sha256:x\nNarSize: 1\n",
			wantErr: "safe nar object path",
		},
		{
			name:    "missing NarHash",
			body:    "StorePath: " + testStorePath + "\nURL: nar/x.nar\nNarSize: 1\n",
			wantErr: "NarHash",
		},
		{
			name:    "bad NarSize",
			body:    "StorePath: " + testStorePath + "\nURL: nar/x.nar\nNarHash: sha256:x\nNarSize: huge\n",
			wantErr: "NarSize",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			rec := httptest.NewRecorder()
			b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/"+testStoreHash+".narinfo", strings.NewReader(tc.body)))
			if rec.Code != http.StatusBadRequest {
				t.Fatalf("status: got %d, want 400; body=%q", rec.Code, rec.Body.String())
			}
			if !strings.Contains(rec.Body.String(), tc.wantErr) {
				t.Fatalf("body %q should mention %q", rec.Body.String(), tc.wantErr)
			}
		})
	}
}

func TestNixStoreBackendUploadCacheInfoFileExists(t *testing.T) {
	staging := t.TempDir()
	if _, err := newNixStoreUploadBackend("ssh://host", staging, nil, slog.Default(), nil); err != nil {
		t.Fatalf("newNixStoreUploadBackend: %v", err)
	}
	data, err := os.ReadFile(filepath.Join(staging, "nix-cache-info"))
	if err != nil {
		t.Fatalf("nix-cache-info missing: %v", err)
	}
	if !bytes.Contains(data, []byte("StoreDir: /nix/store")) {
		t.Fatalf("unexpected nix-cache-info: %q", string(data))
	}
}

func TestNixStoreBackendRejectsTraversalNarinfoPath(t *testing.T) {
	b, staging := newTestNixStoreBackend(t, "", nil)

	body := "StorePath: " + testStorePath + "\nURL: nar/x.nar\nNarHash: sha256:x\nNarSize: 1\n"
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/../etc/passwd.narinfo", strings.NewReader(body)))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("traversal narinfo status: got %d, want 400", rec.Code)
	}

	if _, err := os.Stat(filepath.Join(filepath.Dir(staging), "etc", "passwd.narinfo")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("traversal narinfo escaped staging dir: %v", err)
	}
}

func TestNixStoreBackendRejectsNarinfoFilenameHashMismatch(t *testing.T) {
	b, _ := newTestNixStoreBackend(t, "", nil)
	mustUploadNar(t, b, "x.nar", "x")

	body := "StorePath: " + testStorePath + "\nURL: nar/x.nar\nNarHash: sha256:x\nNarSize: 1\n"
	rec := httptest.NewRecorder()
	b.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/11111111111111111111111111111111.narinfo", strings.NewReader(body)))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("mismatched narinfo status: got %d, want 400; body=%q", rec.Code, rec.Body.String())
	}
	if !strings.Contains(rec.Body.String(), "filename does not match") {
		t.Fatalf("unexpected body: %q", rec.Body.String())
	}
}

func TestParseNarinfoAcceptsLargeReferencesLine(t *testing.T) {
	var refs []string
	for range 12000 {
		refs = append(refs, "0123456789abcdfghijklmnpqrsvwxy-ref")
	}

	body := strings.Join([]string{
		"StorePath: " + testStorePath,
		"URL: nar/x.nar",
		"NarHash: sha256:abc",
		"NarSize: 1",
		"References: " + strings.Join(refs, " "),
		"",
	}, "\n")

	info, err := parseNarinfo(strings.NewReader(body))
	if err != nil {
		t.Fatalf("parseNarinfo failed for large references line: %v", err)
	}
	if info.StorePath != testStorePath {
		t.Fatalf("StorePath: got %q, want %q", info.StorePath, testStorePath)
	}
}
