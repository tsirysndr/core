package db

import (
	"context"
	"path/filepath"
	"testing"

	"tangled.org/core/appview/models"
)

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
