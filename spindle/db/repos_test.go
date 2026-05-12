package db

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func newTestDB(t *testing.T) *DB {
	t.Helper()
	d, err := Make(context.Background(), filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatalf("Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func TestCollapseRepoSiblings_DeletesStaleNullCreatedAtWithDifferentRkey(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'stale-bogus-rkey', ?, null),
		('k', ?, 'fresh-pds-rkey',   ?, '2024-06-01T00:00:00Z')`,
		owner.String(), repoDid.String(),
		owner.String(), repoDid.String()); err != nil {
		t.Fatalf("seed: %v", err)
	}

	n, err := d.CollapseRepoSiblings(owner, repoDid)
	if err != nil {
		t.Fatalf("CollapseRepoSiblings: %v", err)
	}
	if n != 1 {
		t.Errorf("expected 1 stale row deleted, got %d", n)
	}

	var rkey string
	if err := d.QueryRow(`select rkey from repos where owner = ? and repo_did = ?`,
		owner.String(), repoDid.String()).Scan(&rkey); err != nil {
		t.Fatalf("query: %v", err)
	}
	if rkey != "fresh-pds-rkey" {
		t.Errorf("expected fresh row preserved, got rkey=%q", rkey)
	}
}

func TestCollapseRepoSiblings_KeepsNullCreatedAtWhenAlone(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'sole-row', ?, null)`,
		owner.String(), repoDid.String()); err != nil {
		t.Fatalf("seed: %v", err)
	}

	n, err := d.CollapseRepoSiblings(owner, repoDid)
	if err != nil {
		t.Fatalf("CollapseRepoSiblings: %v", err)
	}
	if n != 0 {
		t.Errorf("expected 0 deletions when only NULL row exists, got %d", n)
	}

	var count int
	if err := d.QueryRow(`select count(*) from repos where owner = ?`, owner.String()).Scan(&count); err != nil {
		t.Fatalf("count: %v", err)
	}
	if count != 1 {
		t.Errorf("sole NULL row should survive, got %d remaining", count)
	}
}

func TestCollapseRepoSiblings_OlderTimestampLoses(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'older-rkey',  ?, '2024-01-01T00:00:00Z'),
		('k', ?, 'newer-rkey',  ?, '2024-06-01T00:00:00Z')`,
		owner.String(), repoDid.String(),
		owner.String(), repoDid.String()); err != nil {
		t.Fatalf("seed: %v", err)
	}

	n, err := d.CollapseRepoSiblings(owner, repoDid)
	if err != nil {
		t.Fatalf("CollapseRepoSiblings: %v", err)
	}
	if n != 1 {
		t.Errorf("expected older row collapsed, got %d", n)
	}

	var rkey string
	if err := d.QueryRow(`select rkey from repos where owner = ? and repo_did = ?`,
		owner.String(), repoDid.String()).Scan(&rkey); err != nil {
		t.Fatalf("query: %v", err)
	}
	if rkey != "newer-rkey" {
		t.Errorf("expected newer row preserved, got rkey=%q", rkey)
	}
}

func TestCollapseRepoSiblings_KeepsNullRowWithMatchingRkey(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'matched-rkey', ?, null)`,
		owner.String(), repoDid.String()); err != nil {
		t.Fatalf("seed: %v", err)
	}

	if err := d.AddRepo(Repo{
		Knot:      "k",
		Owner:     owner,
		Rkey:      "matched-rkey",
		RepoDid:   repoDid,
		CreatedAt: "2024-06-01T00:00:00Z",
	}); err != nil {
		t.Fatalf("AddRepo upsert: %v", err)
	}

	if _, err := d.CollapseRepoSiblings(owner, repoDid); err != nil {
		t.Fatalf("CollapseRepoSiblings: %v", err)
	}

	var count int
	if err := d.QueryRow(`select count(*) from repos where owner = ?`, owner.String()).Scan(&count); err != nil {
		t.Fatalf("count: %v", err)
	}
	if count != 1 {
		t.Errorf("upserted row should be the single survivor, got %d", count)
	}
}
