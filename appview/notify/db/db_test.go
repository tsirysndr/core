package db_test

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	appviewdb "tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	notifydb "tangled.org/core/appview/notify/db"
)

func TestNewIssue_DeliversWithNullRkeyCollaborator(t *testing.T) {
	d := setupNotifyTestDB(t)

	ownerDid := "did:plc:boltless"
	repoDid := "did:plc:anemone"
	collabDid := "did:plc:limpet"
	authorDid := "did:plc:akshay"

	repo := seedNotifyRepo(t, d, ownerDid, repoDid)
	insertNullRkeyCollaborator(t, d, ownerDid, collabDid, repoDid)

	issue := seedIssue(t, d, authorDid, repoDid, repo)

	notifier := notifydb.NewDatabaseNotifier(d, nil)
	notifier.NewIssue(context.Background(), issue, nil)

	if got := notificationCount(t, d, ownerDid); got != 1 {
		t.Errorf("repo owner %s: want 1 notification, got %d", ownerDid, got)
	}
	if got := notificationCount(t, d, collabDid); got != 1 {
		t.Errorf("null-rkey collaborator %s: want 1 notification, got %d", collabDid, got)
	}
	if got := notificationCount(t, d, authorDid); got != 0 {
		t.Errorf("issue author %s: want 0 notifications, got %d", authorDid, got)
	}
}

func TestNewPull_DeliversWithNullRkeyCollaborator(t *testing.T) {
	d := setupNotifyTestDB(t)

	ownerDid := "did:plc:boltless"
	repoDid := "did:plc:anemone"
	collabDid := "did:plc:limpet"
	authorDid := "did:plc:akshay"

	seedNotifyRepo(t, d, ownerDid, repoDid)
	insertNullRkeyCollaborator(t, d, ownerDid, collabDid, repoDid)

	pull := seedPull(t, d, authorDid, repoDid)

	notifier := notifydb.NewDatabaseNotifier(d, nil)
	notifier.NewPull(context.Background(), pull)

	if got := notificationCount(t, d, ownerDid); got != 1 {
		t.Errorf("repo owner %s: want 1 notification, got %d", ownerDid, got)
	}
	if got := notificationCount(t, d, collabDid); got != 1 {
		t.Errorf("null-rkey collaborator %s: want 1 notification, got %d", collabDid, got)
	}
	if got := notificationCount(t, d, authorDid); got != 0 {
		t.Errorf("pull author %s: want 0 notifications, got %d", authorDid, got)
	}
}

func setupNotifyTestDB(t *testing.T) *appviewdb.DB {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := appviewdb.Make(context.Background(), path)
	if err != nil {
		t.Fatalf("Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func seedNotifyRepo(t *testing.T, d *appviewdb.DB, ownerDid, repoDid string) *models.Repo {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	repo := &models.Repo{
		Did:     ownerDid,
		Name:    "shell",
		Knot:    "knot.example",
		Rkey:    "shellrkey",
		RepoDid: repoDid,
	}
	if err := appviewdb.AddRepo(tx, repo); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return repo
}

func seedIssue(t *testing.T, d *appviewdb.DB, authorDid, repoDid string, repo *models.Repo) *models.Issue {
	t.Helper()
	issue := &models.Issue{
		Did:     authorDid,
		Rkey:    "issuerkey",
		RepoDid: syntax.DID(repoDid),
		IssueId: 1,
		Title:   "test",
		Body:    "body",
		Open:    true,
		Repo:    repo,
	}
	result, err := d.Exec(
		`insert into issues (did, rkey, repo_did, issue_id, title, body, open) values (?, ?, ?, ?, ?, ?, 1)`,
		issue.Did, issue.Rkey, string(issue.RepoDid), issue.IssueId, issue.Title, issue.Body,
	)
	if err != nil {
		t.Fatalf("insert issue: %v", err)
	}
	id, err := result.LastInsertId()
	if err != nil {
		t.Fatalf("LastInsertId: %v", err)
	}
	issue.Id = id
	return issue
}

func seedPull(t *testing.T, d *appviewdb.DB, authorDid, repoDid string) *models.Pull {
	t.Helper()
	pull := &models.Pull{
		RepoDid:      syntax.DID(repoDid),
		OwnerDid:     authorDid,
		Rkey:         "pullrkey",
		Title:        "test",
		Body:         "body",
		TargetBranch: "main",
		State:        models.PullOpen,
	}
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := appviewdb.PutPull(tx, pull); err != nil {
		t.Fatalf("PutPull: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return pull
}

func insertNullRkeyCollaborator(t *testing.T, d *appviewdb.DB, issuerDid, subjectDid, repoDid string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into collaborators (did, rkey, subject_did, repo_did) values (?, NULL, ?, ?)`,
		issuerDid, subjectDid, repoDid,
	); err != nil {
		t.Fatalf("insert null-rkey collaborator: %v", err)
	}
}

func notificationCount(t *testing.T, d *appviewdb.DB, recipientDid string) int {
	t.Helper()
	var count int
	if err := d.QueryRow(
		`select count(*) from notifications where recipient_did = ?`,
		recipientDid,
	).Scan(&count); err != nil {
		t.Fatalf("count notifications for %s: %v", recipientDid, err)
	}
	return count
}
