package knotserver

import (
	"context"
	"path/filepath"
	"testing"

	"tangled.org/core/knotserver/db"
)

func newTestKnotDB(t *testing.T) *db.DB {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := db.Setup(context.Background(), path)
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	return d
}

func TestAliasResolvesOriginalName(t *testing.T) {
	d := newTestKnotDB(t)
	if err := d.StoreRepoKey("did:plc:repo1", []byte("dummy"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	got, err := d.GetRepoDid("did:plc:akshay", "foo")
	if err != nil {
		t.Fatalf("GetRepoDid: %v", err)
	}
	if got != "did:plc:repo1" {
		t.Errorf("repoDid = %q, want did:plc:repo1", got)
	}
	if _, err := d.GetRepoDid("did:plc:akshay", "Foo"); err == nil {
		t.Error("GetRepoDid with a mismatched-case rkey: got nil error, want failure")
	}
}

func TestAliasUpsertRespectsRevOrdering(t *testing.T) {
	d := newTestKnotDB(t)
	if err := d.StoreRepoKey("did:plc:repo1", []byte("dummy"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := d.UpsertRepoAlias(db.RepoAlias{
		OwnerDid: "did:plc:akshay",
		Rkey:     "bar",
		RepoDid:  "did:plc:repo1",
		Rev:      "3laaaaaaaaaab",
	}); err != nil {
		t.Fatalf("UpsertRepoAlias bar: %v", err)
	}

	_, current, err := d.CurrentRkey("did:plc:repo1")
	if err != nil {
		t.Fatalf("CurrentRkey: %v", err)
	}
	if current != "bar" {
		t.Errorf("current rkey = %q, want bar", current)
	}

	fooDid, err := d.GetRepoDid("did:plc:akshay", "foo")
	if err != nil || fooDid != "did:plc:repo1" {
		t.Errorf("old rkey lookup: got (%q, %v), want did:plc:repo1", fooDid, err)
	}
	barDid, err := d.GetRepoDid("did:plc:akshay", "bar")
	if err != nil || barDid != "did:plc:repo1" {
		t.Errorf("new rkey lookup: got (%q, %v), want did:plc:repo1", barDid, err)
	}
}
