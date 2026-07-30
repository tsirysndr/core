package keys

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotserver/db"
)

const (
	didBoltless = "did:plc:boltless"
	keyAlpha    = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAlphaAlphaAlphaAlphaAlphaAlphaAlphaAlpha01"
	keyBravo    = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIABravoBravoBravoBravoBravoBravoBravoBravo02"
)

func TestFetchAndStore_EmptyResponseDoesNotWipe(t *testing.T) {
	store := newKeyStore(t)
	seedKey(t, store, didBoltless, "seed", keyAlpha)

	srv := pdsServer(t, map[string]*comatproto.RepoListRecords_Output{
		"": page(""),
	})
	defer srv.Close()

	if err := FetchAndStore(context.Background(), pdsDirectory(srv.URL), store, didBoltless); err != nil {
		t.Fatalf("FetchAndStore: %v", err)
	}

	owners := ownersByKey(t, store)
	if _, ok := owners[keyAlpha]; !ok {
		t.Fatalf("seeded key was wiped by an empty fetch response, owners=%v", owners)
	}
	if got := len(owners); got != 1 {
		t.Errorf("stored %d keys, want 1", got)
	}
}

func TestFetchAndStore_ReplacesExistingKeys(t *testing.T) {
	store := newKeyStore(t)
	seedKey(t, store, didBoltless, "stale", keyAlpha)

	srv := pdsServer(t, map[string]*comatproto.RepoListRecords_Output{
		"": page("", pubkeyRecord(didBoltless, "fresh", keyBravo)),
	})
	defer srv.Close()

	if err := FetchAndStore(context.Background(), pdsDirectory(srv.URL), store, didBoltless); err != nil {
		t.Fatalf("FetchAndStore: %v", err)
	}

	owners := ownersByKey(t, store)
	if _, ok := owners[keyAlpha]; ok {
		t.Errorf("stale key %q survived a full replace", keyAlpha)
	}
	if _, ok := owners[keyBravo]; !ok {
		t.Errorf("fresh key %q was not stored", keyBravo)
	}
	if got := len(owners); got != 1 {
		t.Errorf("stored %d keys, want 1", got)
	}
}

func TestFetchAndStore_PaginatesAcrossPages(t *testing.T) {
	store := newKeyStore(t)
	if err := db.AddDid(store, didBoltless); err != nil {
		t.Fatalf("AddDid: %v", err)
	}

	srv := pdsServer(t, map[string]*comatproto.RepoListRecords_Output{
		"":     page("next", pubkeyRecord(didBoltless, "r1", keyAlpha)),
		"next": page("", pubkeyRecord(didBoltless, "r2", keyBravo)),
	})
	defer srv.Close()

	if err := FetchAndStore(context.Background(), pdsDirectory(srv.URL), store, didBoltless); err != nil {
		t.Fatalf("FetchAndStore: %v", err)
	}

	owners := ownersByKey(t, store)
	if _, ok := owners[keyAlpha]; !ok {
		t.Errorf("first-page key %q missing", keyAlpha)
	}
	if _, ok := owners[keyBravo]; !ok {
		t.Errorf("second-page key %q missing", keyBravo)
	}
	if got := len(owners); got != 2 {
		t.Errorf("stored %d keys, want 2", got)
	}
}

func newKeyStore(t *testing.T) *db.DB {
	t.Helper()
	store, err := db.Setup(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	return store
}

func seedKey(t *testing.T, store *db.DB, did syntax.DID, rkey syntax.RecordKey, key string) {
	t.Helper()
	if err := db.AddDid(store, did.String()); err != nil {
		t.Fatalf("AddDid: %v", err)
	}
	if err := store.UpsertPublicKey(db.PublicKey{
		Did:       did,
		Rkey:      rkey,
		PublicKey: tangled.PublicKey{Key: key, CreatedAt: "2026-06-20T00:00:00Z"},
	}); err != nil {
		t.Fatalf("UpsertPublicKey: %v", err)
	}
}

func ownersByKey(t *testing.T, store *db.DB) map[string]string {
	t.Helper()
	rows, err := store.GetAllPublicKeys()
	if err != nil {
		t.Fatalf("GetAllPublicKeys: %v", err)
	}
	return foldOwners(rows, map[string]string{})
}

func foldOwners(rows []db.PublicKey, acc map[string]string) map[string]string {
	if len(rows) == 0 {
		return acc
	}
	acc[rows[0].Key] = rows[0].Did.String()
	return foldOwners(rows[1:], acc)
}

func page(cursor string, records ...*comatproto.RepoListRecords_Record) *comatproto.RepoListRecords_Output {
	out := &comatproto.RepoListRecords_Output{Records: records}
	if cursor != "" {
		out.Cursor = &cursor
	}
	return out
}

func pubkeyRecord(did, rkey, key string) *comatproto.RepoListRecords_Record {
	return &comatproto.RepoListRecords_Record{
		Uri: "at://" + did + "/" + tangled.PublicKeyNSID + "/" + rkey,
		Cid: "bafyreib2rxk3rybk3aobmv5cjuql3bm2twh4jo5uxgr3rxbd3xrnp5z5cy",
		Value: &lexutil.LexiconTypeDecoder{
			Val: &tangled.PublicKey{Key: key, CreatedAt: "2026-06-20T00:00:00Z"},
		},
	}
}

func pdsServer(t *testing.T, pages map[string]*comatproto.RepoListRecords_Output) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		cursor := r.URL.Query().Get("cursor")
		out, ok := pages[cursor]
		if !ok {
			t.Errorf("listRecords requested with unexpected cursor %q", cursor)
			http.Error(w, "no such page", http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		if err := json.NewEncoder(w).Encode(out); err != nil {
			t.Errorf("encoding listRecords response: %v", err)
		}
	}))
}

func pdsDirectory(url string) idresolver.MockDirectory {
	return idresolver.MockDirectory{Ident: &identity.Identity{
		Services: map[string]identity.ServiceEndpoint{
			"atproto_pds": {Type: "AtprotoPersonalDataServer", URL: url},
		},
	}}
}
