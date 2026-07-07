package xrpc

import (
	"os"
	"path/filepath"
	"testing"
)

const (
	resolveOwnerDid  = "did:plc:akshay"
	resolveRepoDid   = "did:plc:squid"
	resolveStoredKey = "squidbot"
)

func setupResolveRepo(t *testing.T) (*Xrpc, string) {
	t.Helper()
	x := newTestXrpc(t)
	scanPath := t.TempDir()
	x.Config.Repo.ScanPath = scanPath

	if err := x.Db.StoreRepoKey(resolveRepoDid, []byte("k256"), resolveOwnerDid, resolveStoredKey); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := os.MkdirAll(filepath.Join(scanPath, resolveRepoDid), 0o755); err != nil {
		t.Fatalf("mkdir repo dir: %v", err)
	}
	return x, scanPath
}

func TestResolveRepoDID_PrefersRepoOverName(t *testing.T) {
	x, scanPath := setupResolveRepo(t)

	repo := resolveRepoDid
	gotDid, gotPath, err := x.resolveRepoDID(&repo, resolveOwnerDid, "SquidBot")
	if err != nil {
		t.Fatalf("resolveRepoDID with repo set: %v", err)
	}
	if gotDid != resolveRepoDid {
		t.Errorf("repoDid = %q, want %q", gotDid, resolveRepoDid)
	}
	if want := filepath.Join(scanPath, resolveRepoDid); gotPath != want {
		t.Errorf("repoPath = %q, want %q", gotPath, want)
	}
}

func TestResolveRepoDID_RejectsMalformedRepoDid(t *testing.T) {
	x, _ := setupResolveRepo(t)

	malformed := "not-a-did"
	if _, _, err := x.resolveRepoDID(&malformed, resolveOwnerDid, resolveStoredKey); err == nil {
		t.Fatal("resolveRepoDID with malformed repo DID: got nil error, want failure")
	}
}

func TestResolveRepoDID_UnknownRepoDidDoesNotFallBackToName(t *testing.T) {
	x, _ := setupResolveRepo(t)

	unknown := "did:plc:limpet"
	if _, _, err := x.resolveRepoDID(&unknown, resolveOwnerDid, resolveStoredKey); err == nil {
		t.Fatal("resolveRepoDID with unknown repo DID and resolvable name: got nil error, want failure")
	}
}

func TestResolveRepoDID_NameFallbackIsCaseSensitive(t *testing.T) {
	x, _ := setupResolveRepo(t)

	if _, _, err := x.resolveRepoDID(nil, resolveOwnerDid, "SquidBot"); err == nil {
		t.Fatal("resolveRepoDID with mismatched-case name: got nil error, want failure")
	}

	empty := ""
	if _, _, err := x.resolveRepoDID(&empty, resolveOwnerDid, "SquidBot"); err == nil {
		t.Fatal("resolveRepoDID with empty repo and mismatched-case name: got nil error, want failure")
	}

	gotDid, _, err := x.resolveRepoDID(nil, resolveOwnerDid, resolveStoredKey)
	if err != nil {
		t.Fatalf("resolveRepoDID with exact-case name: %v", err)
	}
	if gotDid != resolveRepoDid {
		t.Errorf("repoDid = %q, want %q", gotDid, resolveRepoDid)
	}
}
