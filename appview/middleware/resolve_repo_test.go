package middleware

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
)

func TestResolveRepo_RenameAlias(t *testing.T) {
	const ownerDid, handle, knot = "did:plc:boltless", "boltless.dev", "knot1.tangled.sh"
	cases := []struct {
		name                    string
		repoName, repoRkey, did string
		alias, reqRepo          string
		wantLocation            string // empty => expect the repo to be served without a redirect
	}{
		{"alias equal to live slug is served, not looped", "anemone", "3mpxmsvicr2zn", "did:plc:anemone", "anemone", "anemone", ""},
		{"genuine rename still redirects", "whelk", "whelk", "did:plc:whelk", "conch", "conch", "/boltless.dev/whelk"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "test.db"))
			if err != nil {
				t.Fatalf("Make: %v", err)
			}
			t.Cleanup(func() { d.Close() })

			tx, err := d.Begin()
			if err != nil {
				t.Fatalf("Begin: %v", err)
			}
			if err := db.AddRepo(tx, &models.Repo{Did: ownerDid, Name: tc.repoName, Knot: knot, Rkey: tc.repoRkey, RepoDid: tc.did}); err != nil {
				t.Fatalf("AddRepo: %v", err)
			}
			if err := tx.Commit(); err != nil {
				t.Fatalf("Commit: %v", err)
			}
			if err := db.RecordRepoRename(d, ownerDid, tc.alias, tc.did); err != nil {
				t.Fatalf("RecordRepoRename: %v", err)
			}

			req := httptest.NewRequest(http.MethodGet, "/"+handle+"/"+tc.reqRepo, nil)
			rctx := chi.NewRouteContext()
			rctx.URLParams.Add("user", handle)
			rctx.URLParams.Add("repo", tc.reqRepo)
			ctx := context.WithValue(req.Context(), chi.RouteCtxKey, rctx)
			ctx = context.WithValue(ctx, "resolvedId", identity.Identity{DID: syntax.DID(ownerDid), Handle: syntax.Handle(handle)})

			rec := httptest.NewRecorder()
			served := false
			next := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { served = true })
			mw := Middleware{db: d, logger: slog.New(slog.NewTextHandler(io.Discard, nil))}
			mw.ResolveRepo()(next).ServeHTTP(rec, req.WithContext(ctx))

			if tc.wantLocation == "" {
				if !served {
					t.Fatalf("expected repo served, got code=%d Location=%q", rec.Code, rec.Header().Get("Location"))
				}
				return
			}
			if served {
				t.Fatal("expected a redirect, but the repo was served")
			}
			if rec.Code != http.StatusMovedPermanently {
				t.Fatalf("got %d, want 301", rec.Code)
			}
			if got := rec.Header().Get("Location"); got != tc.wantLocation {
				t.Errorf("Location = %q, want %q", got, tc.wantLocation)
			}
		})
	}
}
