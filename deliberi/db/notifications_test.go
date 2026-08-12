package db

import (
	"context"
	"path/filepath"
	"testing"

	"tangled.org/core/deliberi/models"
	"tangled.org/core/orm"
)

func testDB(t *testing.T) *DB {
	t.Helper()
	d, err := Make(context.Background(), filepath.Join(t.TempDir(), "x.db"))
	if err != nil {
		t.Fatalf("Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

// one source record fans out to a row per recipient; read state is per row
// (recipient_did, at_uri), so marking one recipient's row read leaves another
// recipient's untouched.
func TestNotificationsIsolatedPerRecipient(t *testing.T) {
	d := testDB(t)
	const uri = "at://did:plc:author/sh.tangled.feed.comment/abc"

	for _, did := range []string{"did:plc:alice", "did:plc:bob"} {
		if err := CreateNotification(d, &models.Notification{
			RecipientDid: did,
			AtUri:        uri,
			Type:         models.NotificationTypeIssueCommented,
			ActorDid:     "did:plc:author",
		}); err != nil {
			t.Fatalf("CreateNotification %s: %v", did, err)
		}
	}

	if err := MarkRead(d, "did:plc:alice", uri, true); err != nil {
		t.Fatalf("MarkRead: %v", err)
	}

	aliceUnread, _ := CountNotifications(d, "did:plc:alice", orm.FilterEq("read", 0))
	bobUnread, _ := CountNotifications(d, "did:plc:bob", orm.FilterEq("read", 0))
	if aliceUnread != 0 {
		t.Fatalf("alice unread = %d, want 0", aliceUnread)
	}
	if bobUnread != 1 {
		t.Fatalf("bob unread = %d, want 1 (must not inherit alice's read)", bobUnread)
	}
}

// CreateNotification dedupes on (recipient_did, at_uri): a firehose replay or a
// user both subscribed and mentioned yields a single row.
func TestCreateNotificationDedupe(t *testing.T) {
	d := testDB(t)
	const uri = "at://did:plc:repo/sh.tangled.repo.issue/abc"

	for range 2 {
		if err := CreateNotification(d, &models.Notification{
			RecipientDid: "did:plc:alice",
			AtUri:        uri,
			Type:         models.NotificationTypeIssueCreated,
			ActorDid:     "did:plc:author",
		}); err != nil {
			t.Fatalf("CreateNotification: %v", err)
		}
	}

	count, _ := CountNotifications(d, "did:plc:alice")
	if count != 1 {
		t.Fatalf("row count = %d, want 1 (deduped)", count)
	}
}
