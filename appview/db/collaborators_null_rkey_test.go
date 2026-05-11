package db

import (
	"database/sql"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func TestGetCollaborators_NullRkey(t *testing.T) {
	d := newTestDB(t)

	seedRepo(t, d, "did:plc:boltless", "knot.example", "anemone", "anemone", "did:plc:anemone")

	_, err := d.Exec(
		`insert into collaborators (did, rkey, subject_did, repo_did) values (?, NULL, ?, ?)`,
		"did:plc:boltless", "did:plc:akshay", "did:plc:anemone",
	)
	if err != nil {
		t.Fatalf("insert: %v", err)
	}

	collabs, err := GetCollaborators(d, orm.FilterEq("repo_did", "did:plc:anemone"))
	if err != nil {
		t.Fatalf("GetCollaborators: %v", err)
	}
	if len(collabs) != 1 {
		t.Fatalf("want 1 collab, got %d", len(collabs))
	}
	if collabs[0].Rkey.Valid {
		t.Fatalf("expected NULL rkey to scan as !Valid, got %+v", collabs[0].Rkey)
	}
}

func TestAddCollaborator_DuplicateUpdatesRkey(t *testing.T) {
	d := newTestDB(t)

	seedRepo(t, d, "did:plc:boltless", "knot.example", "anemone", "anemone", "did:plc:anemone")

	collab := func(rkey string) models.Collaborator {
		return models.Collaborator{
			Did:        syntax.DID("did:plc:boltless"),
			Rkey:       sql.NullString{String: rkey, Valid: true},
			SubjectDid: syntax.DID("did:plc:akshay"),
			RepoDid:    syntax.DID("did:plc:anemone"),
		}
	}

	if err := AddCollaborator(d, collab("rkey-first")); err != nil {
		t.Fatalf("first AddCollaborator: %v", err)
	}
	if err := AddCollaborator(d, collab("rkey-second")); err != nil {
		t.Fatalf("second AddCollaborator: %v", err)
	}

	collabs, err := GetCollaborators(d, orm.FilterEq("repo_did", "did:plc:anemone"))
	if err != nil {
		t.Fatalf("GetCollaborators: %v", err)
	}
	if len(collabs) != 1 {
		t.Fatalf("want 1 collab after dup add, got %d", len(collabs))
	}
	if collabs[0].Rkey.String != "rkey-second" {
		t.Fatalf("want rkey-second, got %q", collabs[0].Rkey.String)
	}
}

func TestEnqueuePdsRewritesForRepo_SkipsNullRkeyCollab(t *testing.T) {
	d := newTestDB(t)

	seedRepo(t, d, "did:plc:boltless", "knot.example", "anemone", "anemone", "did:plc:anemone")

	if _, err := d.Exec(
		`insert into collaborators (did, rkey, subject_did, repo_did) values (?, NULL, ?, ?)`,
		"did:plc:boltless", "did:plc:akshay", "did:plc:anemone",
	); err != nil {
		t.Fatalf("insert: %v", err)
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()

	if err := EnqueuePdsRewritesForRepo(tx, "did:plc:anemone", "at://did:plc:boltless/sh.tangled.repo/anemone"); err != nil {
		t.Fatalf("EnqueuePdsRewritesForRepo: %v", err)
	}

	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}
