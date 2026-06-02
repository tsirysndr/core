package knotserver

import (
	"context"
	"io"
	"log/slog"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
)

const (
	bfOwner  = "did:plc:akshay"
	bfCollab = "did:plc:boltless"
	bfRepo   = "did:plc:limpet"
)

func newBackfillEnv(t *testing.T) (*db.DB, *rbac.Enforcer) {
	t.Helper()
	dir := t.TempDir()
	d, err := db.Setup(context.Background(), filepath.Join(dir, "knot.db"))
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	e, err := rbac.NewEnforcer(filepath.Join(dir, "rbac.db"))
	if err != nil {
		t.Fatalf("NewEnforcer: %v", err)
	}
	if err := e.AddKnot(rbac.ThisServer); err != nil {
		t.Fatalf("AddKnot: %v", err)
	}
	if err := e.AddKnotOwner(rbac.ThisServer, bfOwner); err != nil {
		t.Fatalf("AddKnotOwner: %v", err)
	}
	return d, e
}

func seedCasbinRepo(t *testing.T, d *db.DB, e *rbac.Enforcer, repoDid string, collaborators ...string) {
	t.Helper()
	if err := d.StoreRepoKey(repoDid, []byte("signing"), bfOwner, "reponame"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := e.AddRepo(bfOwner, rbac.ThisServer, repoDid); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	for _, c := range collaborators {
		if err := e.AddCollaborator(c, rbac.ThisServer, repoDid); err != nil {
			t.Fatalf("AddCollaborator %s: %v", c, err)
		}
	}
}

func runBackfill(t *testing.T, d *db.DB, e *rbac.Enforcer) {
	t.Helper()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	if err := BackfillCollaborators(context.Background(), d, e, logger, true); err != nil {
		t.Fatalf("BackfillCollaborators: %v", err)
	}
}

func runMemberBackfill(t *testing.T, d *db.DB, e *rbac.Enforcer) {
	t.Helper()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	if err := BackfillKnotMembers(context.Background(), d, e, bfOwner, logger); err != nil {
		t.Fatalf("BackfillKnotMembers: %v", err)
	}
}

func TestBackfillCollaborators_FoldsCasbinAndExcludesOwner(t *testing.T) {
	d, e := newBackfillEnv(t)
	seedCasbinRepo(t, d, e, bfRepo, bfCollab)

	runBackfill(t, d, e)

	list, _, err := db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators: %v", err)
	}
	if len(list) != 1 {
		t.Fatalf("collaborators = %+v, want exactly one (owner must be excluded)", list)
	}
	if list[0].Subject != syntax.DID(bfCollab) {
		t.Errorf("subject = %s, want %s", list[0].Subject, bfCollab)
	}
	if list[0].AddedBy != syntax.DID(bfOwner) {
		t.Errorf("addedBy = %s, want owner %s", list[0].AddedBy, bfOwner)
	}
}

func TestBackfillCollaborators_OneTimeAndNonDestructive(t *testing.T) {
	d, e := newBackfillEnv(t)
	seedCasbinRepo(t, d, e, bfRepo, bfCollab)

	runBackfill(t, d, e)

	if err := e.AddCollaborator("did:plc:scallop", rbac.ThisServer, bfRepo); err != nil {
		t.Fatalf("post-migration casbin add: %v", err)
	}
	runBackfill(t, d, e)

	list, _, err := db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators: %v", err)
	}
	if len(list) != 1 {
		t.Fatalf("collaborators = %d, want 1; backfill must run once and never resurrect later casbin state", len(list))
	}
	if list[0].Subject != syntax.DID(bfCollab) {
		t.Errorf("subject = %s, want original %s preserved", list[0].Subject, bfCollab)
	}
}

func TestBackfillCollaborators_LeavesMembersUntouched(t *testing.T) {
	d, e := newBackfillEnv(t)
	seedCasbinRepo(t, d, e, bfRepo, bfCollab)

	owner := syntax.DID(bfOwner)
	member := syntax.DID("did:plc:whelk")
	if err := db.AddKnotMemberDirect(d, owner, member); err != nil {
		t.Fatalf("seed member: %v", err)
	}
	if err := e.AddKnotMember(rbac.ThisServer, member.String()); err != nil {
		t.Fatalf("seed member acl: %v", err)
	}

	runBackfill(t, d, e)

	members, _, err := db.ListKnotMembers(d, db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListKnotMembers: %v", err)
	}
	if len(members) != 1 || members[0].Subject != member {
		t.Fatalf("members = %+v, want the seeded member preserved", members)
	}

	collabs, _, err := db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators: %v", err)
	}
	if len(collabs) != 1 || collabs[0].Subject != syntax.DID(bfCollab) {
		t.Errorf("collaborators = %+v, want only the casbin collaborator", collabs)
	}
}

func TestBackfillCollaborators_UnmarkedRunDefersMarker(t *testing.T) {
	d, e := newBackfillEnv(t)
	seedCasbinRepo(t, d, e, bfRepo, bfCollab)

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	if err := BackfillCollaborators(context.Background(), d, e, logger, false); err != nil {
		t.Fatalf("unmarked backfill: %v", err)
	}

	list, _, err := db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators: %v", err)
	}
	if len(list) != 1 {
		t.Fatalf("collaborators = %d, want 1 after unmarked run", len(list))
	}
	applied, err := d.IsMigrationApplied(collaboratorBackfillMigration)
	if err != nil {
		t.Fatalf("IsMigrationApplied: %v", err)
	}
	if applied {
		t.Fatal("unmarked run must not write the migration marker")
	}

	if err := e.AddCollaborator("did:plc:scallop", rbac.ThisServer, bfRepo); err != nil {
		t.Fatalf("late casbin add: %v", err)
	}
	runBackfill(t, d, e)

	list, _, err = db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators after marked run: %v", err)
	}
	if len(list) != 2 {
		t.Errorf("collaborators = %d, want 2; the marked rerun must fold late casbin state", len(list))
	}
	if applied, _ := d.IsMigrationApplied(collaboratorBackfillMigration); !applied {
		t.Error("marked run did not write the migration marker")
	}
}

func TestBackfillKnotMembers_FoldsCasbinAndExcludesOwner(t *testing.T) {
	d, e := newBackfillEnv(t)
	member := syntax.DID("did:plc:whelk")
	if err := e.AddKnotMember(rbac.ThisServer, member.String()); err != nil {
		t.Fatalf("seed casbin member: %v", err)
	}

	runMemberBackfill(t, d, e)

	members, _, err := db.ListKnotMembers(d, db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListKnotMembers: %v", err)
	}
	if len(members) != 1 {
		t.Fatalf("members = %+v, want exactly one; the owner must be excluded", members)
	}
	if members[0].Subject != member {
		t.Errorf("subject = %s, want %s", members[0].Subject, member)
	}
	if members[0].Did != syntax.DID(bfOwner) {
		t.Errorf("added by = %s, want owner %s", members[0].Did, bfOwner)
	}
}

func TestBackfillKnotMembers_OneTimeAndNonDestructive(t *testing.T) {
	d, e := newBackfillEnv(t)
	member := syntax.DID("did:plc:whelk")
	if err := e.AddKnotMember(rbac.ThisServer, member.String()); err != nil {
		t.Fatalf("seed casbin member: %v", err)
	}

	runMemberBackfill(t, d, e)

	if err := e.AddKnotMember(rbac.ThisServer, "did:plc:scallop"); err != nil {
		t.Fatalf("post-migration casbin add: %v", err)
	}
	runMemberBackfill(t, d, e)

	members, _, err := db.ListKnotMembers(d, db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListKnotMembers: %v", err)
	}
	if len(members) != 1 || members[0].Subject != member {
		t.Fatalf("members = %+v, want only the original member; backfill must run once", members)
	}
	if applied, _ := d.IsMigrationApplied(knotMemberBackfillMigration); !applied {
		t.Error("member backfill did not write its migration marker")
	}
}

func TestBackfillCollaborators_EmptyMarksApplied(t *testing.T) {
	d, e := newBackfillEnv(t)
	seedCasbinRepo(t, d, e, bfRepo)

	runBackfill(t, d, e)

	applied, err := d.IsMigrationApplied(collaboratorBackfillMigration)
	if err != nil {
		t.Fatalf("IsMigrationApplied: %v", err)
	}
	if !applied {
		t.Fatal("migration not marked applied after a zero-collaborator backfill; it would re-scan every boot")
	}

	if err := e.AddCollaborator(bfCollab, rbac.ThisServer, bfRepo); err != nil {
		t.Fatalf("post-migration casbin add: %v", err)
	}
	runBackfill(t, d, e)

	list, _, err := db.ListCollaborators(d, syntax.DID(bfRepo), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil {
		t.Fatalf("ListCollaborators: %v", err)
	}
	if len(list) != 0 {
		t.Errorf("collaborators = %d, want 0; an applied migration must not fold later casbin state", len(list))
	}
}
