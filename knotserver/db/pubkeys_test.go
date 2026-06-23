package db

import (
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
)

const (
	didBoltless = "did:plc:boltless"
	didAkshay   = "did:plc:akshay"
	keyShared   = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwwmlNQEh5NdGL4ERWj3uXWXylXsB8fPnO5frkl2sps"
	keyRotated  = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAj/UuveywM4LZdjbcsH5LVmXhu8VX5jdUR6UdEQFGBo"
	keyOther    = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBqWLXSkuE6AUkAwXThOsudIGqMV/u4ZnE8yTd6DSpoR"
)

func TestUpsertPublicKey_GlobalUniqueness(t *testing.T) {
	d := newTestDB(t)
	addDid(t, d, didBoltless)
	addDid(t, d, didAkshay)

	if err := d.UpsertPublicKey(pubKey(didBoltless, "r-first", keyShared)); err != nil {
		t.Fatalf("first upsert: %v", err)
	}
	if err := d.UpsertPublicKey(pubKey(didAkshay, "r-second", keyShared)); err != nil {
		t.Fatalf("second upsert: %v", err)
	}

	owners := ownersByKey(t, d)
	if got := len(owners); got != 1 {
		t.Fatalf("stored %d copies of the key, want 1, owners=%v", got, owners)
	}
	if owners[keyShared] != didBoltless {
		t.Errorf("key registered to %q, want %q (first writer keeps it)", owners[keyShared], didBoltless)
	}
}

func TestUpsertPublicKey_RotatesAtSameRkey(t *testing.T) {
	d := newTestDB(t)
	addDid(t, d, didBoltless)

	if err := d.UpsertPublicKey(pubKey(didBoltless, "rotate", keyShared)); err != nil {
		t.Fatalf("upsert old: %v", err)
	}
	if err := d.UpsertPublicKey(pubKey(didBoltless, "rotate", keyRotated)); err != nil {
		t.Fatalf("upsert new: %v", err)
	}

	owners := ownersByKey(t, d)
	if _, ok := owners[keyShared]; ok {
		t.Errorf("old key %q survived rotation at the same rkey", keyShared)
	}
	if _, ok := owners[keyRotated]; !ok {
		t.Errorf("rotated key %q not stored", keyRotated)
	}
	if got := len(owners); got != 1 {
		t.Errorf("stored %d keys, want 1", got)
	}
}

func TestDeletePublicKeyByRkey(t *testing.T) {
	d := newTestDB(t)
	addDid(t, d, didBoltless)

	if err := d.UpsertPublicKey(pubKey(didBoltless, "keep", keyShared)); err != nil {
		t.Fatalf("upsert keep: %v", err)
	}
	if err := d.UpsertPublicKey(pubKey(didBoltless, "drop", keyOther)); err != nil {
		t.Fatalf("upsert drop: %v", err)
	}

	if err := d.DeletePublicKeyByRkey(didBoltless, ""); err != nil {
		t.Fatalf("delete with empty rkey: %v", err)
	}
	if got := len(ownersByKey(t, d)); got != 2 {
		t.Fatalf("empty rkey deleted %d rows, want a no-op leaving 2", 2-got)
	}

	if err := d.DeletePublicKeyByRkey(didBoltless, "drop"); err != nil {
		t.Fatalf("delete drop: %v", err)
	}

	owners := ownersByKey(t, d)
	if _, ok := owners[keyOther]; ok {
		t.Errorf("key at rkey %q was not deleted", "drop")
	}
	if _, ok := owners[keyShared]; !ok {
		t.Errorf("delete removed the wrong key, %q is gone", keyShared)
	}
}

func TestInsertPublicKey_SkipsEmptyKey(t *testing.T) {
	d := newTestDB(t)
	addDid(t, d, didBoltless)

	if err := d.UpsertPublicKey(pubKey(didBoltless, "empty", "")); err != nil {
		t.Fatalf("upsert empty key: %v", err)
	}

	if got := len(ownersByKey(t, d)); got != 0 {
		t.Errorf("stored %d rows for an empty key, want 0", got)
	}
}

func addDid(t *testing.T, d *DB, did string) {
	t.Helper()
	if err := AddDid(d, did); err != nil {
		t.Fatalf("AddDid(%q): %v", did, err)
	}
}

func pubKey(did syntax.DID, rkey syntax.RecordKey, key string) PublicKey {
	return PublicKey{
		Did:       did,
		Rkey:      rkey,
		PublicKey: tangled.PublicKey{Key: key, CreatedAt: "2026-06-20T00:00:00Z"},
	}
}

func ownersByKey(t *testing.T, d *DB) map[string]string {
	t.Helper()
	rows, err := d.GetAllPublicKeys()
	if err != nil {
		t.Fatalf("GetAllPublicKeys: %v", err)
	}
	return foldOwners(rows, map[string]string{})
}

func foldOwners(rows []PublicKey, acc map[string]string) map[string]string {
	if len(rows) == 0 {
		return acc
	}
	acc[rows[0].Key] = rows[0].Did.String()
	return foldOwners(rows[1:], acc)
}
