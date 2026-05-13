package reporesolver

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

func TestExtractCurrentDir(t *testing.T) {
	tests := []struct {
		path string
		want string
	}{
		{"/@user/repo/blob/main/docs/README.md", "docs"},
		{"/@user/repo/blob/main/README.md", "."},
		{"/@user/repo/tree/main/docs", "docs"},
		{"/@user/repo/tree/main/docs/", "docs"},
		{"/@user/repo/tree/main", "."},
	}

	for _, tt := range tests {
		if got := extractCurrentDir(tt.path); got != tt.want {
			t.Errorf("extractCurrentDir(%q) = %q, want %q", tt.path, got, tt.want)
		}
	}
}

func TestCanonicalRepoPath(t *testing.T) {
	cases := []struct {
		name   string
		handle string
		repo   *models.Repo
		want   string
	}{
		{"name preferred", "boltless.dev", &models.Repo{Name: "anemone", Rkey: "3kabc"}, "boltless.dev/anemone"},
		{"name equals rkey", "boltless.dev", &models.Repo{Name: "clam", Rkey: "clam"}, "boltless.dev/clam"},
		{"empty name uses rkey", "akshay.dev", &models.Repo{Rkey: "limpet"}, "akshay.dev/limpet"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := CanonicalRepoPath(c.handle, c.repo); got != c.want {
				t.Errorf("CanonicalRepoPath = %q, want %q", got, c.want)
			}
		})
	}
}

func reqWithChiParams(user, repo string) *http.Request {
	r := httptest.NewRequest("GET", "/", nil)
	rctx := chi.NewRouteContext()
	rctx.URLParams.Add("user", user)
	rctx.URLParams.Add("repo", repo)
	return r.WithContext(context.WithValue(r.Context(), chi.RouteCtxKey, rctx))
}

func TestGetBaseRepoPath_DoesNotVoluntaryRedirectToRepoDid(t *testing.T) {
	r := reqWithChiParams("@boltless.dev", "anemone")
	repo := &models.Repo{
		Did:     "did:plc:boltless",
		Name:    "anemone",
		Rkey:    "3kabcxyz",
		RepoDid: "did:plc:anemone",
	}
	got := GetBaseRepoPath(r, repo)
	want := "@boltless.dev/anemone"
	if got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}

func TestGetBaseRepoPath_HonorsUrlParams(t *testing.T) {
	r := reqWithChiParams("did:plc:akshay", "limpet")
	repo := &models.Repo{Did: "did:plc:akshay", Name: "limpet", Rkey: "limpet"}
	if got, want := GetBaseRepoPath(r, repo), "did:plc:akshay/limpet"; got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}

func TestGetBaseRepoPath_NoParamsPrefersName(t *testing.T) {
	r := httptest.NewRequest("GET", "/", nil)
	repo := &models.Repo{Did: "did:plc:akshay", Name: "scallop", Rkey: "3koldtid"}
	got := GetBaseRepoPath(r, repo)
	want := "did:plc:akshay/scallop"
	if got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}

func TestGetBaseRepoPath_NoParamsNoNameFallsToRepoIdentifier(t *testing.T) {
	r := httptest.NewRequest("GET", "/", nil)
	repo := &models.Repo{Did: "did:plc:akshay", Rkey: "3koldtid", RepoDid: "did:plc:scallop"}
	if got, want := GetBaseRepoPath(r, repo), "did:plc:scallop"; got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}

func reqWithResolvedId(handle, did string) *http.Request {
	r := reqWithChiParams(did, "3koldtid")
	id := identity.Identity{
		DID:    syntax.DID(did),
		Handle: syntax.Handle(handle),
	}
	return r.WithContext(context.WithValue(r.Context(), "resolvedId", id))
}

func TestGetBaseRepoPath_PrefersResolvedHandleOverChiDid(t *testing.T) {
	r := reqWithResolvedId("boltless.dev", "did:plc:boltless")
	repo := &models.Repo{Did: "did:plc:boltless", Name: "anemone", Rkey: "3kabcxyz"}
	got := GetBaseRepoPath(r, repo)
	want := "boltless.dev/anemone"
	if got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}

func TestGetBaseRepoPath_InvalidHandleFallsThroughToChi(t *testing.T) {
	r := reqWithChiParams("did:plc:boltless", "limpet")
	id := identity.Identity{
		DID:    syntax.DID("did:plc:boltless"),
		Handle: syntax.HandleInvalid,
	}
	r = r.WithContext(context.WithValue(r.Context(), "resolvedId", id))
	repo := &models.Repo{Did: "did:plc:boltless", Name: "limpet", Rkey: "limpet"}
	if got, want := GetBaseRepoPath(r, repo), "did:plc:boltless/limpet"; got != want {
		t.Errorf("GetBaseRepoPath = %q, want %q", got, want)
	}
}
