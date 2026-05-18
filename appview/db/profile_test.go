package db

import (
	"database/sql"
	"errors"
	"testing"
)

func seedProfile(t *testing.T, d *DB, did string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into profile (did, description, include_bluesky, location, preferred_handle)
		 values (?, ?, ?, ?, ?)`,
		did, "hi", 0, "", "boltless.bsky.social",
	); err != nil {
		t.Fatalf("seed profile: %v", err)
	}
	if _, err := d.Exec(
		`insert into profile_links (did, link) values (?, ?)`,
		did, "https://boltless.example/blog",
	); err != nil {
		t.Fatalf("seed profile_links: %v", err)
	}
	if _, err := d.Exec(
		`insert into profile_stats (did, kind) values (?, ?)`,
		did, "open-pull-request-count",
	); err != nil {
		t.Fatalf("seed profile_stats: %v", err)
	}
	if _, err := d.Exec(
		`insert into profile_pinned_repositories (did, pin) values (?, ?)`,
		did, "did:plc:limpet",
	); err != nil {
		t.Fatalf("seed profile_pinned_repositories: %v", err)
	}
}

func countRows(t *testing.T, d *DB, query string, args ...any) int {
	t.Helper()
	var n int
	if err := d.QueryRow(query, args...).Scan(&n); err != nil {
		t.Fatalf("count: %v", err)
	}
	return n
}

func TestDeleteProfile_CascadesAllChildTables(t *testing.T) {
	d := newTestDB(t)
	const did = "did:plc:boltless"
	seedProfile(t, d, did)

	if got := countRows(t, d, `select count(*) from profile where did = ?`, did); got != 1 {
		t.Fatalf("pre: profile rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_links where did = ?`, did); got != 1 {
		t.Fatalf("pre: profile_links rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_stats where did = ?`, did); got != 1 {
		t.Fatalf("pre: profile_stats rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_pinned_repositories where did = ?`, did); got != 1 {
		t.Fatalf("pre: profile_pinned_repositories rows = %d, want 1", got)
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := DeleteProfile(tx, did); err != nil {
		t.Fatalf("DeleteProfile: %v", err)
	}

	if got := countRows(t, d, `select count(*) from profile where did = ?`, did); got != 0 {
		t.Errorf("post: profile rows = %d, want 0", got)
	}
	if got := countRows(t, d, `select count(*) from profile_links where did = ?`, did); got != 0 {
		t.Errorf("post: profile_links rows = %d, want 0 (cascade)", got)
	}
	if got := countRows(t, d, `select count(*) from profile_stats where did = ?`, did); got != 0 {
		t.Errorf("post: profile_stats rows = %d, want 0 (cascade)", got)
	}
	if got := countRows(t, d, `select count(*) from profile_pinned_repositories where did = ?`, did); got != 0 {
		t.Errorf("post: profile_pinned_repositories rows = %d, want 0 (cascade)", got)
	}
}

func TestDeleteProfile_NoRowsIsNoop(t *testing.T) {
	d := newTestDB(t)

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := DeleteProfile(tx, "did:plc:akshay"); err != nil {
		t.Errorf("DeleteProfile on missing did: %v, want nil", err)
	}
}

func TestDeleteProfile_LeavesOtherDidsAlone(t *testing.T) {
	d := newTestDB(t)
	seedProfile(t, d, "did:plc:boltless")
	seedProfile(t, d, "did:plc:akshay")

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := DeleteProfile(tx, "did:plc:boltless"); err != nil {
		t.Fatalf("DeleteProfile: %v", err)
	}

	if got := countRows(t, d, `select count(*) from profile where did = ?`, "did:plc:akshay"); got != 1 {
		t.Errorf("other profile should survive: rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_links where did = ?`, "did:plc:akshay"); got != 1 {
		t.Errorf("other profile_links should survive: rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_stats where did = ?`, "did:plc:akshay"); got != 1 {
		t.Errorf("other profile_stats should survive: rows = %d, want 1", got)
	}
	if got := countRows(t, d, `select count(*) from profile_pinned_repositories where did = ?`, "did:plc:akshay"); got != 1 {
		t.Errorf("other profile_pinned_repositories should survive: rows = %d, want 1", got)
	}
}

func TestGetPreferredHandle_AfterDeleteReturnsNoRows(t *testing.T) {
	d := newTestDB(t)
	const did = "did:plc:boltless"
	seedProfile(t, d, did)

	h, err := GetPreferredHandle(d, did)
	if err != nil {
		t.Fatalf("GetPreferredHandle pre-delete: %v", err)
	}
	if string(h) != "boltless.bsky.social" {
		t.Fatalf("handle = %q, want %q", h, "boltless.bsky.social")
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := DeleteProfile(tx, did); err != nil {
		t.Fatalf("DeleteProfile: %v", err)
	}

	_, err = GetPreferredHandle(d, did)
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("GetPreferredHandle post-delete: err = %v, want sql.ErrNoRows", err)
	}
}
