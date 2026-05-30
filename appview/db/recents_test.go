package db

import (
	"fmt"
	"testing"

	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func insertRecentLink(t *testing.T, d *DB, userDid string, linkType models.RecentLinkType, target, visited string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into recent_links (user_did, link_type, target, visited) values (?, ?, ?, ?)`,
		userDid, string(linkType), target, visited,
	); err != nil {
		t.Fatalf("insertRecentLink %q: %v", target, err)
	}
}

func TestUpsertRecentLink_LimitFive(t *testing.T) {
	d := newTestDB(t)
	const userDid = "did:plc:akshay"

	for i := range 6 {
		target := fmt.Sprintf("%d-repo-did", i)
		if err := UpsertRecentLink(d, userDid, models.RecentLinkTypeRepo, target); err != nil {
			t.Fatalf("UpsertRecentLink %d: %v", i, err)
		}
	}

	if got := countRows(t, d, "select count(*) from recent_links where user_did = ?", userDid); got != 5 {
		t.Errorf("recent_links count = %d, want 5", got)
	}
}

func TestUpsertRecentLink_DeduplicatesAndUpdatesTimestamp(t *testing.T) {
	d := newTestDB(t)
	const userDid = "did:plc:akshay"
	const target = "at://did:plc:akshay/sh.tangled.repo/myrepo"

	insertRecentLink(t, d, userDid, models.RecentLinkTypeIssue, target, "2024-01-01T00:00:00Z")

	if err := UpsertRecentLink(d, userDid, models.RecentLinkTypeIssue, target); err != nil {
		t.Fatalf("upsert: %v", err)
	}

	if got := countRows(t, d, "select count(*) from recent_links where user_did = ? and target = ?", userDid, target); got != 1 {
		t.Errorf("expected 1 row after duplicate upsert, got %d", got)
	}

	var visited string
	if err := d.QueryRow("select visited from recent_links where user_did = ? and target = ?", userDid, target).Scan(&visited); err != nil {
		t.Fatalf("scan visited: %v", err)
	}
	if visited <= "2024-01-01T00:00:00Z" {
		t.Errorf("visited not updated: got %q, want > %q", visited, "2024-01-01T00:00:00Z")
	}
}

func TestGetRecentLinks_LessThanFive(t *testing.T) {
	d := newTestDB(t)
	const userDid = "did:plc:akshay"

	for i := range 6 {
		name := fmt.Sprintf("repo-%d", i)
		insertRecentLink(t, d, userDid, models.RecentLinkTypeRepo, name, "2024-01-01T00:00:00Z")
	}

	links, err := GetRecentLinks(d, orm.FilterEq("user_did", userDid))
	if err != nil {
		t.Fatalf("GetRecentLinks: %v", err)
	}

	if len(links) > 5 {
		t.Errorf("GetRecentLinks returned %d links, want <5", len(links))
	}
}

func TestGetRecentLinks_OrderedByMostRecent(t *testing.T) {
	d := newTestDB(t)
	const userDid = "did:plc:akshay"

	insertRecentLink(t, d, userDid, models.RecentLinkTypeRepo, "repo-first", "2024-01-01T00:00:00Z")
	insertRecentLink(t, d, userDid, models.RecentLinkTypeRepo, "repo-second", "2024-01-02T00:00:00Z")
	insertRecentLink(t, d, userDid, models.RecentLinkTypeRepo, "repo-third", "2024-01-03T00:00:00Z")

	links, err := GetRecentLinks(d, orm.FilterEq("user_did", userDid))
	if err != nil {
		t.Fatalf("GetRecentLinks: %v", err)
	}

	if len(links) != 3 {
		t.Fatalf("expected 3 links, got %d", len(links))
	}
	if links[0].Target != "repo-third" {
		t.Errorf("links[0].Target = %q, want %q", links[0].Target, "repo-third")
	}
	if links[2].Target != "repo-first" {
		t.Errorf("links[2].Target = %q, want %q", links[2].Target, "repo-first")
	}
}
