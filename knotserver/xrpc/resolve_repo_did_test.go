package xrpc

import (
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/rbac"
)

func setupResolveRepo(t *testing.T) (*Xrpc, string) {
	t.Helper()
	x, _ := newACLXrpc(t)
	scanPath := t.TempDir()
	x.Config.Repo.ScanPath = scanPath
	seedRepo(t, x)
	if err := os.MkdirAll(filepath.Join(scanPath, aclRepoDid), 0o755); err != nil {
		t.Fatalf("mkdir repo dir: %v", err)
	}
	return x, scanPath
}

func TestResolveRepoDID(t *testing.T) {
	x, scanPath := setupResolveRepo(t)

	got, err := x.resolveRepoDID(aclRepoDid)
	if err != nil {
		t.Fatalf("resolveRepoDID: %v", err)
	}
	if got.did != aclRepoDid || got.owner != aclOwner {
		t.Errorf("resolved %q owned by %q, want %q owned by %q", got.did, got.owner, aclRepoDid, aclOwner)
	}
	if want := filepath.Join(scanPath, aclRepoDid); got.path != want {
		t.Errorf("repoPath = %q, want %q", got.path, want)
	}

	rejected := map[string]string{
		"a malformed repo DID":                   "not-a-did",
		"an empty repo DID":                      "",
		"a repo DID that this knot doesn't host": "did:plc:conch",
		"an owner and name instead of a DID":     aclOwner + "/reponame",
	}
	for name, repo := range rejected {
		t.Run(name, func(t *testing.T) {
			if _, err := x.resolveRepoDID(repo); err == nil {
				t.Fatalf("resolveRepoDID(%q): got nil error, want failure", repo)
			}
		})
	}
}

func TestPushableRepoDID(t *testing.T) {
	x, _ := setupResolveRepo(t)
	if err := x.Enforcer.AddRepo(aclOwner, rbac.ThisServer, "did:plc:conch"); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}

	if _, denial := x.pushableRepoDID(aclOwner, aclRepoDid); denial != nil {
		t.Fatalf("owner denied: %v (status %d), want the repo", denial.err, denial.status)
	}

	cases := map[string]struct {
		actor syntax.DID
		repo  string
		want  int
	}{
		"we'll 401 a stranger": {aclSubject, aclRepoDid, http.StatusUnauthorized},
		"we'll 400 a repo that this knot doesn't host before the ACL read": {aclOwner, "did:plc:conch", http.StatusBadRequest},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			_, denial := x.pushableRepoDID(tc.actor, tc.repo)
			if denial == nil || denial.status != tc.want {
				t.Fatalf("denial = %v, want status %d", denial, tc.want)
			}
		})
	}
}
