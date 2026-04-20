package xrpc

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"path/filepath"
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
)

func newTestXrpc(t *testing.T) *Xrpc {
	t.Helper()
	d, err := db.Setup(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	return &Xrpc{
		Db:     d,
		Config: &config.Config{Server: config.Server{Hostname: "knot.example", MaxResponseKB: 5120}},
		Logger: slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
}

func TestRepoDescribeRepo_ReturnsOwner(t *testing.T) {
	x := newTestXrpc(t)
	if err := x.Db.StoreRepoKey("did:plc:repo1", []byte("dummy"), "did:plc:akshay", "myrepo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}

	req := httptest.NewRequest(http.MethodGet, "/xrpc/sh.tangled.repo.describeRepo?repoDid=did:plc:repo1", nil)
	rec := httptest.NewRecorder()
	x.RepoDescribeRepo(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}

	var out tangled.RepoDescribeRepo_Output
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if out.RepoDid != "did:plc:repo1" {
		t.Errorf("RepoDid = %q", out.RepoDid)
	}
	if out.OwnerDid != "did:plc:akshay" {
		t.Errorf("OwnerDid = %q, want did:plc:akshay", out.OwnerDid)
	}
	if out.Rkey != "myrepo" {
		t.Errorf("Rkey = %q, want myrepo", out.Rkey)
	}
}

func TestRepoDescribeRepo_UnknownRepoDidReturns404(t *testing.T) {
	x := newTestXrpc(t)

	req := httptest.NewRequest(http.MethodGet, "/xrpc/sh.tangled.repo.describeRepo?repoDid=did:plc:unknown", nil)
	rec := httptest.NewRecorder()
	x.RepoDescribeRepo(rec, req)

	if rec.Code != http.StatusNotFound {
		t.Errorf("status = %d, want 404", rec.Code)
	}
}

func TestRepoDescribeRepo_MissingParamReturns400(t *testing.T) {
	x := newTestXrpc(t)

	req := httptest.NewRequest(http.MethodGet, "/xrpc/sh.tangled.repo.describeRepo", nil)
	rec := httptest.NewRecorder()
	x.RepoDescribeRepo(rec, req)

	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}

func TestRepoDescribeRepo_MalformedDidParamReturns400(t *testing.T) {
	x := newTestXrpc(t)
	u := "/xrpc/sh.tangled.repo.describeRepo?repoDid=" + url.QueryEscape("notadid")
	req := httptest.NewRequest(http.MethodGet, u, nil)
	rec := httptest.NewRecorder()
	x.RepoDescribeRepo(rec, req)
	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}
