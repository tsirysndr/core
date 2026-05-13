package middleware

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/models"
)

func runCanonicalize(t *testing.T, method, urlPath, urlUser, urlRepo, handle string, repo *models.Repo) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, urlPath, nil)
	rctx := chi.NewRouteContext()
	rctx.URLParams.Add("user", urlUser)
	rctx.URLParams.Add("repo", urlRepo)
	ctx := context.WithValue(req.Context(), chi.RouteCtxKey, rctx)
	id := identity.Identity{
		DID:    syntax.DID("did:plc:boltless"),
		Handle: syntax.Handle(handle),
	}
	ctx = context.WithValue(ctx, "resolvedId", id)
	ctx = context.WithValue(ctx, "repo", repo)
	req = req.WithContext(ctx)

	rec := httptest.NewRecorder()
	called := false
	next := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		called = true
		w.WriteHeader(http.StatusOK)
	})
	mw := Middleware{}
	mw.CanonicalizeRepoURL()(next).ServeHTTP(rec, req)
	if rec.Code == http.StatusFound {
		if called {
			t.Errorf("middleware both issued 302 and invoked next handler")
		}
	}
	return rec
}

func TestCanonicalize_CanonicalUrlPassesThrough(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/boltless.dev/anemone/issues", "boltless.dev", "anemone", "boltless.dev", repo)
	if rec.Code != http.StatusOK {
		t.Errorf("canonical URL got %d, want 200; Location=%q", rec.Code, rec.Header().Get("Location"))
	}
}

func TestCanonicalize_EmptyNameUsesRkeyAsSlug(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/boltless.dev/anemone/pulls", "boltless.dev", "anemone", "boltless.dev", repo)
	if rec.Code != http.StatusOK {
		t.Errorf("rkey-as-slug canonical URL got %d, want 200; Location=%q", rec.Code, rec.Header().Get("Location"))
	}
}

func TestCanonicalize_EmptyNameOwnerDidRedirectsToHandleRkey(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/did:plc:boltless/anemone/pulls", "did:plc:boltless", "anemone", "boltless.dev", repo)
	if rec.Code != http.StatusFound {
		t.Fatalf("got %d, want 302", rec.Code)
	}
	if got, want := rec.Header().Get("Location"), "/boltless.dev/anemone/pulls"; got != want {
		t.Errorf("Location = %q, want %q", got, want)
	}
}

func TestCanonicalize_HandleSlashTIDRedirectsToName(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "3kabcxyz"}
	rec := runCanonicalize(t, "GET", "/boltless.dev/3kabcxyz/issues", "boltless.dev", "3kabcxyz", "boltless.dev", repo)
	if rec.Code != http.StatusFound {
		t.Fatalf("got %d, want 302", rec.Code)
	}
	if got, want := rec.Header().Get("Location"), "/boltless.dev/anemone/issues"; got != want {
		t.Errorf("Location = %q, want %q", got, want)
	}
}

func TestCanonicalize_OwnerDidRedirectsToHandle(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/did:plc:boltless/anemone/pulls/3", "did:plc:boltless", "anemone", "boltless.dev", repo)
	if rec.Code != http.StatusFound {
		t.Fatalf("got %d, want 302", rec.Code)
	}
	if got, want := rec.Header().Get("Location"), "/boltless.dev/anemone/pulls/3"; got != want {
		t.Errorf("Location = %q, want %q", got, want)
	}
}

func TestCanonicalize_OwnerDidAndTIDRedirectsToCanonical(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "3kabcxyz"}
	rec := runCanonicalize(t, "GET", "/did:plc:boltless/3kabcxyz", "did:plc:boltless", "3kabcxyz", "boltless.dev", repo)
	if rec.Code != http.StatusFound {
		t.Fatalf("got %d, want 302", rec.Code)
	}
	if got, want := rec.Header().Get("Location"), "/boltless.dev/anemone"; got != want {
		t.Errorf("Location = %q, want %q", got, want)
	}
}

func TestCanonicalize_PreservesQueryString(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/did:plc:boltless/anemone/issues?state=closed&page=2", "did:plc:boltless", "anemone", "boltless.dev", repo)
	if got, want := rec.Header().Get("Location"), "/boltless.dev/anemone/issues?state=closed&page=2"; got != want {
		t.Errorf("Location = %q, want %q", got, want)
	}
}

func TestCanonicalize_PostNotRedirected(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "POST", "/did:plc:boltless/anemone/issues", "did:plc:boltless", "anemone", "boltless.dev", repo)
	if rec.Code != http.StatusOK {
		t.Errorf("POST on non-canonical URL got %d, want 200; Location=%q", rec.Code, rec.Header().Get("Location"))
	}
}

func TestCanonicalize_InvalidHandlePassesThrough(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/did:plc:boltless/anemone", "did:plc:boltless", "anemone", string(syntax.HandleInvalid), repo)
	if rec.Code != http.StatusOK {
		t.Errorf("invalid handle got %d, want 200; Location=%q", rec.Code, rec.Header().Get("Location"))
	}
}

func TestCanonicalize_DotGitSuffixStripped(t *testing.T) {
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "anemone"}
	rec := runCanonicalize(t, "GET", "/boltless.dev/anemone.git/", "boltless.dev", "anemone.git", "boltless.dev", repo)
	if rec.Code != http.StatusOK {
		t.Errorf(".git on canonical name got %d, want 200; Location=%q", rec.Code, rec.Header().Get("Location"))
	}
}
