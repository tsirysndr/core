package db

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
)

func TestRemoveReposByKnotCascadesEntities(t *testing.T) {
	d := newTestDB(t)

	knot := "kelp.example"
	repo := seedRepo(t, d, "did:plc:akshay", knot, "anemone", "anemone", "did:plc:anemone")

	starNotif := &models.Notification{
		RecipientDid: "did:plc:akshay",
		ActorDid:     "did:plc:boltless",
		Type:         models.NotificationTypeRepoStarred,
		EntityType:   "repo",
		EntityId:     repo.RepoAt().String(),
		RepoId:       &repo.Id,
	}
	if err := CreateNotification(d, starNotif); err != nil {
		t.Fatalf("CreateNotification repo: %v", err)
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	issue := &models.Issue{
		Did:     "did:plc:akshay",
		Rkey:    "issue1",
		RepoDid: syntax.DID(repo.RepoDid),
		Title:   "title",
		Body:    "body",
		Open:    true,
	}
	if err := PutIssue(tx, issue); err != nil {
		t.Fatalf("PutIssue: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	issueNotif := &models.Notification{
		RecipientDid: "did:plc:akshay",
		ActorDid:     "did:plc:boltless",
		Type:         models.NotificationTypeIssueCommented,
		EntityType:   "issue",
		EntityId:     issue.AtUri().String(),
		IssueId:      &issue.Id,
	}
	if err := CreateNotification(d, issueNotif); err != nil {
		t.Fatalf("CreateNotification issue: %v", err)
	}

	if err := RemoveReposByKnot(d, knot); err != nil {
		t.Fatalf("RemoveReposByKnot: %v", err)
	}

	if got := countRows(t, d, "select count(*) from repos where knot = ?", knot); got != 0 {
		t.Errorf("repos remaining: got %d, want 0", got)
	}
	if got := countRows(t, d, "select count(*) from issues where repo_did = ?", repo.RepoDid); got != 0 {
		t.Errorf("issues remaining: got %d, want 0", got)
	}
	if got := countRows(t, d, "select count(*) from notifications"); got != 0 {
		t.Errorf("notifications remaining: got %d, want 0", got)
	}
}

func TestMakeReopenPreservesOrphanData(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reopen.db")

	d, err := Make(context.Background(), path)
	if err != nil {
		t.Fatalf("first Make: %v", err)
	}

	repo := seedRepo(t, d, "did:plc:akshay", "ghost.example", "anemone", "anemone", "did:plc:anemone")
	notif := &models.Notification{
		RecipientDid: "did:plc:akshay",
		ActorDid:     "did:plc:boltless",
		Type:         models.NotificationTypeRepoStarred,
		EntityType:   "repo",
		EntityId:     repo.RepoAt().String(),
		RepoId:       &repo.Id,
	}
	if err := CreateNotification(d, notif); err != nil {
		t.Fatalf("CreateNotification: %v", err)
	}
	if err := d.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	d2, err := Make(context.Background(), path)
	if err != nil {
		t.Fatalf("second Make: %v", err)
	}
	t.Cleanup(func() { d2.Close() })

	if got := countRows(t, d2, "select count(*) from repos where knot = ?", "ghost.example"); got != 1 {
		t.Errorf("orphan repo lost across reopen: got %d, want 1", got)
	}
	if got := countRows(t, d2, "select count(*) from notifications"); got != 1 {
		t.Errorf("notification lost across reopen: got %d, want 1", got)
	}
}
