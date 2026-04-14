package db

import (
	"context"
	"database/sql"
	"errors"
	"path/filepath"
	"testing"

	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func newTestDB(t *testing.T) *DB {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := Make(context.Background(), path)
	if err != nil {
		t.Fatalf("Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func seedRepo(t *testing.T, d *DB, did, knot, name, rkey, repoDid string) *models.Repo {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	repo := &models.Repo{
		Did:     did,
		Name:    name,
		Knot:    knot,
		Rkey:    rkey,
		RepoDid: repoDid,
	}
	if err := AddRepo(tx, repo); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return repo
}

func TestRenameRepo_HappyPath(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()

	if err := RenameRepo(tx, "did:plc:akshay", "foo", "bar", "Bar"); err != nil {
		t.Fatalf("RenameRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	got, err := GetRepoByDid(d, "did:plc:repo1")
	if err != nil {
		t.Fatalf("GetRepoByDid: %v", err)
	}
	if got.Rkey != "bar" {
		t.Errorf("rkey = %q, want %q", got.Rkey, "bar")
	}
	if got.Name != "Bar" {
		t.Errorf("name = %q, want %q", got.Name, "Bar")
	}
}

func TestUpdateRepoDisplayName_HappyPath(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	if err := UpdateRepoDisplayName(d, "did:plc:akshay", "foo", "Foo"); err != nil {
		t.Fatalf("UpdateRepoDisplayName: %v", err)
	}

	got, err := GetRepoByDid(d, "did:plc:repo1")
	if err != nil {
		t.Fatalf("GetRepoByDid: %v", err)
	}
	if got.Name != "Foo" {
		t.Errorf("name = %q, want %q", got.Name, "Foo")
	}
	if got.Rkey != "foo" {
		t.Errorf("rkey should be unchanged but got %q, want %q", got.Rkey, "foo")
	}
}

func TestRecordAndLookupRepoRename(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "bar", "rkey1", "did:plc:repo1")

	if err := RecordRepoRename(d, "did:plc:akshay", "foo", "did:plc:repo1"); err != nil {
		t.Fatalf("RecordRepoRename: %v", err)
	}

	repo, err := LookupRepoRename(d, "did:plc:akshay", "foo")
	if err != nil {
		t.Fatalf("LookupRepoRename: %v", err)
	}
	if repo.RepoDid != "did:plc:repo1" {
		t.Errorf("repoDid = %q, want %q", repo.RepoDid, "did:plc:repo1")
	}
	if repo.Name != "bar" {
		t.Errorf("name = %q, want %q", repo.Name, "bar")
	}
}

func TestLookupRepoRename_MultipleOldNamesResolveToCurrent(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "baz", "baz", "did:plc:repo1")

	if err := RecordRepoRename(d, "did:plc:akshay", "foo", "did:plc:repo1"); err != nil {
		t.Fatalf("record foo: %v", err)
	}
	if err := RecordRepoRename(d, "did:plc:akshay", "bar", "did:plc:repo1"); err != nil {
		t.Fatalf("record bar: %v", err)
	}

	for _, oldName := range []string{"foo", "bar"} {
		repo, err := LookupRepoRename(d, "did:plc:akshay", oldName)
		if err != nil {
			t.Fatalf("lookup %q: %v", oldName, err)
		}
		if repo.Name != "baz" {
			t.Errorf("lookup %q: name = %q, want %q", oldName, repo.Name, "baz")
		}
	}
}

func TestRecordRepoRename_UpsertRefreshesTarget(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "current", "rkey1", "did:plc:repo1")
	seedRepo(t, d, "did:plc:akshay", "knot.example", "other", "rkey2", "did:plc:repo2")

	if err := RecordRepoRename(d, "did:plc:akshay", "shared", "did:plc:repo1"); err != nil {
		t.Fatalf("first record: %v", err)
	}
	if err := RecordRepoRename(d, "did:plc:akshay", "shared", "did:plc:repo2"); err != nil {
		t.Fatalf("second record: %v", err)
	}

	repo, err := LookupRepoRename(d, "did:plc:akshay", "shared")
	if err != nil {
		t.Fatalf("LookupRepoRename: %v", err)
	}
	if repo.RepoDid != "did:plc:repo2" {
		t.Errorf("latest record should win: repoDid = %q, want %q", repo.RepoDid, "did:plc:repo2")
	}
}

func TestLookupRepoRename_StaleSelfHeal(t *testing.T) {
	d := newTestDB(t)

	if err := RecordRepoRename(d, "did:plc:akshay", "foo", "did:plc:ghost"); err != nil {
		t.Fatalf("RecordRepoRename: %v", err)
	}

	_, err := LookupRepoRename(d, "did:plc:akshay", "foo")
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("target should be gone and fall through to 404: err = %v, want sql.ErrNoRows", err)
	}
}

func TestLookupRepoRename_NoRow(t *testing.T) {
	d := newTestDB(t)

	_, err := LookupRepoRename(d, "did:plc:akshay", "nothing")
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("err = %v, want sql.ErrNoRows", err)
	}
}

func TestDuplicateRkeyUnderSameDID_Rejected(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "myrepo", "myrepo", "did:plc:repo1")

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()

	err = AddRepo(tx, &models.Repo{
		Did:     "did:plc:akshay",
		Name:    "myrepo",
		Knot:    "knot.example",
		Rkey:    "myrepo",
		RepoDid: "did:plc:repo2",
	})
	if err == nil {
		t.Fatal("expected unique violation for duplicate (did, rkey), got nil")
	}
	if !orm.IsUniqueViolation(err) {
		t.Errorf("err = %v, want unique violation", err)
	}
}

func TestRenameRepo_OldRkeyRowGone(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "old", "old", "did:plc:repo1")

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()

	if err := RenameRepo(tx, "did:plc:akshay", "old", "new", "New"); err != nil {
		t.Fatalf("RenameRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	got, err := GetRepoByDid(d, "did:plc:repo1")
	if err != nil {
		t.Fatalf("GetRepoByDid: %v", err)
	}
	if got.Rkey != "new" {
		t.Errorf("rkey = %q, want %q", got.Rkey, "new")
	}

	var dummy int
	err = d.QueryRow(`select 1 from repos where did = ? and rkey = ?`, "did:plc:akshay", "old").Scan(&dummy)
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("old rkey row should be gone, got err = %v", err)
	}
}

func TestRenameRepo_PipelineRenamed(t *testing.T) {
	d := newTestDB(t)
	seedRepo(t, d, "did:plc:akshay", "knot.example", "old", "old", "did:plc:repo1")

	if _, err := d.Exec(
		`insert into triggers (kind) values (?)`, "push",
	); err != nil {
		t.Fatalf("seed trigger: %v", err)
	}
	if _, err := d.Exec(
		`insert into pipelines (rkey, knot, repo_owner, repo_name, sha, trigger_id, repo_did)
		 values (?, ?, ?, ?, ?, ?, ?)`,
		"pipe1", "knot.example", "did:plc:akshay", "old",
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 1, "did:plc:repo1",
	); err != nil {
		t.Fatalf("seed pipeline: %v", err)
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()

	if err := RenameRepo(tx, "did:plc:akshay", "old", "new", "New"); err != nil {
		t.Fatalf("RenameRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	var repoName string
	if err := d.QueryRow(
		`select repo_name from pipelines where repo_owner = ? and rkey = ?`,
		"did:plc:akshay", "pipe1",
	).Scan(&repoName); err != nil {
		t.Fatalf("query pipeline: %v", err)
	}
	if repoName != "new" {
		t.Errorf("pipeline repo_name = %q, want %q", repoName, "new")
	}
}
