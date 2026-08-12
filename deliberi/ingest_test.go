package deliberi

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	deldb "tangled.org/core/deliberi/db"
	models "tangled.org/core/deliberi/models"
)

type fakeResolver struct {
	dids []string
	err  error

	// repo owner lookups, keyed by repo did.
	owners    map[string]string
	repoNames map[string]string
	ownerErr  error
	// ownerCalls counts RepoOwner calls, to prove cache hits skip the network.
	ownerCalls *int
}

func (f fakeResolver) ListRecipients(ctx context.Context, uri string) ([]string, error) {
	return f.dids, f.err
}

func (f fakeResolver) RepoOwner(ctx context.Context, repoDid string) (string, string, error) {
	if f.ownerCalls != nil {
		*f.ownerCalls++
	}
	if f.ownerErr != nil {
		return "", "", f.ownerErr
	}
	return f.owners[repoDid], f.repoNames[repoDid], nil
}

func starEvent(t *testing.T, actorDid, repoDid, rkey string) *jmodels.Event {
	t.Helper()
	raw, err := json.Marshal(tangled.FeedStar{
		CreatedAt: "2026-01-01T00:00:00Z",
		Subject: &tangled.FeedStar_Subject{
			FeedStar_Repo: &tangled.FeedStar_Repo{Did: repoDid},
		},
	})
	if err != nil {
		t.Fatalf("marshal star: %v", err)
	}
	return &jmodels.Event{
		Did:  actorDid,
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationCreate,
			Collection: tangled.FeedStarNSID,
			RKey:       rkey,
			Record:     raw,
		},
	}
}

func newTestIngester(t *testing.T, r recipientResolver) *Ingester {
	t.Helper()
	database, err := deldb.Make(context.Background(), filepath.Join(t.TempDir(), "x.db"))
	if err != nil {
		t.Fatalf("make db: %v", err)
	}
	t.Cleanup(func() { database.Close() })
	return &Ingester{
		db:         database,
		recipients: r,
		logger:     slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
}

func countFor(t *testing.T, i *Ingester, did string) int64 {
	t.Helper()
	n, err := deldb.CountNotifications(i.db, did)
	if err != nil {
		t.Fatalf("count: %v", err)
	}
	return n
}

func TestNotifyEntitySubscriberGetsRow(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:sub"}})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil)
	if got := countFor(t, i, "did:sub"); got != 1 {
		t.Fatalf("subscriber rows = %d, want 1", got)
	}
}

func TestActorNeverNotified(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:actor"}})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", []string{"did:actor"})
	if got := countFor(t, i, "did:actor"); got != 0 {
		t.Fatalf("actor rows = %d, want 0", got)
	}
}

func TestMentionDeliveredOnResolverError(t *testing.T) {
	i := newTestIngester(t, fakeResolver{err: io.ErrUnexpectedEOF})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", []string{"did:mention"})
	if got := countFor(t, i, "did:mention"); got != 1 {
		t.Fatalf("mention rows = %d, want 1", got)
	}
}

func TestCreateNotificationDedupe(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:sub"}})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil)
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil)
	if got := countFor(t, i, "did:sub"); got != 1 {
		t.Fatalf("deduped rows = %d, want 1", got)
	}
}

func TestStarNotifiesRepoOwner(t *testing.T) {
	calls := 0
	i := newTestIngester(t, fakeResolver{ownerCalls: &calls})
	const repoDid = "did:plc:therepo"
	const ownerDid = "did:plc:bob"
	defer func() {
		if calls != 0 {
			t.Errorf("RepoOwner calls = %d, want 0 (cache hit must not hit bobbin)", calls)
		}
	}()

	// Seed the repo DID → owner DID mapping (what the repo handler now does).
	if err := deldb.PutRepoName(i.db, repoDid, ownerDid, "my-repo"); err != nil {
		t.Fatalf("PutRepoName: %v", err)
	}

	// Construct a firehose event for Alice starring Bob's repo.
	raw, err := json.Marshal(tangled.FeedStar{
		CreatedAt: "2026-01-01T00:00:00Z",
		Subject: &tangled.FeedStar_Subject{
			FeedStar_Repo: &tangled.FeedStar_Repo{Did: repoDid},
		},
	})
	if err != nil {
		t.Fatalf("marshal star: %v", err)
	}
	ev := &jmodels.Event{
		Did:  "did:plc:alice",
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationCreate,
			Collection: tangled.FeedStarNSID,
			RKey:       "star1",
			Record:     raw,
		},
	}

	if err := i.process(context.Background(), ev); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := countFor(t, i, ownerDid); got != 1 {
		t.Fatalf("owner rows = %d, want 1", got)
	}
	if got := countFor(t, i, repoDid); got != 0 {
		t.Fatalf("repo DID rows = %d, want 0 (notification must go to owner, not repo)", got)
	}
	if got := countFor(t, i, "did:plc:alice"); got != 0 {
		t.Fatalf("actor rows = %d, want 0 (star author must not self-notify)", got)
	}
}

func TestStarResolvesOwnerFromBobbinOnCacheMiss(t *testing.T) {
	const repoDid = "did:plc:therepo"
	const ownerDid = "did:plc:bob"

	// nothing cached: the owner must come from bobbin.
	i := newTestIngester(t, fakeResolver{
		owners:    map[string]string{repoDid: ownerDid},
		repoNames: map[string]string{repoDid: "my-repo"},
	})

	if err := i.process(context.Background(), starEvent(t, "did:plc:alice", repoDid, "star1")); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := countFor(t, i, ownerDid); got != 1 {
		t.Fatalf("owner rows = %d, want 1", got)
	}
	// the lookup must be cached, so the next star on this repo is a local hit.
	if got := deldb.GetRepoOwner(i.db, repoDid); got != ownerDid {
		t.Errorf("cached owner = %q, want %q", got, ownerDid)
	}
	if got := deldb.GetRepoName(i.db, repoDid); got != "my-repo" {
		t.Errorf("cached name = %q, want %q", got, "my-repo")
	}
}

func TestStarKeepsCachedNameWhenRecordIsNameless(t *testing.T) {
	const repoDid = "did:plc:therepo"
	const ownerDid = "did:plc:bob"

	// a row with a name but no owner, as migrated rows can have.
	i := newTestIngester(t, fakeResolver{owners: map[string]string{repoDid: ownerDid}})
	if err := deldb.PutRepoName(i.db, repoDid, "", "my-repo"); err != nil {
		t.Fatalf("PutRepoName: %v", err)
	}

	if err := i.process(context.Background(), starEvent(t, "did:plc:alice", repoDid, "star1")); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := deldb.GetRepoName(i.db, repoDid); got != "my-repo" {
		t.Errorf("cached name = %q, want %q (nameless record must not blank it)", got, "my-repo")
	}
}

func TestStarSkipsWhenOwnerUnresolvable(t *testing.T) {
	// nothing cached and bobbin is unreachable, so the star handler should skip.
	i := newTestIngester(t, fakeResolver{ownerErr: io.ErrUnexpectedEOF})
	const repoDid = "did:plc:therepo"

	if err := i.process(context.Background(), starEvent(t, "did:plc:alice", repoDid, "star1")); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := countFor(t, i, "did:plc:bob"); got != 0 {
		t.Fatalf("bob rows = %d, want 0 (unresolvable owner => skip)", got)
	}
}

func TestRepoHandlerSeedsOwner(t *testing.T) {
	// Verify that the ingester's repo processing stores the correct mapping:
	// repo_did → owner_did and name.
	// This test exercises the same code path as the repo NSID case in process().
	db, err := deldb.Make(context.Background(), filepath.Join(t.TempDir(), "x.db"))
	if err != nil {
		t.Fatalf("make db: %v", err)
	}
	t.Cleanup(func() { db.Close() })

	const ownerDid = "did:plc:owner"
	const repoDid = "did:plc:therepo"
	const repoName = "my-repo"

	if err := deldb.PutRepoName(db, repoDid, ownerDid, repoName); err != nil {
		t.Fatalf("PutRepoName: %v", err)
	}

	if got := deldb.GetRepoOwner(db, repoDid); got != ownerDid {
		t.Fatalf("GetRepoOwner = %q, want %q", got, ownerDid)
	}
	if got := deldb.GetRepoName(db, repoDid); got != repoName {
		t.Fatalf("GetRepoName = %q, want %q", got, repoName)
	}
	// Owner DID alone should NOT resolve as a repo name (it's not the key).
	if got := deldb.GetRepoName(db, ownerDid); got != "" {
		t.Fatalf("GetRepoName(ownerDid) = %q, want empty (owner is not the cache key)", got)
	}
}

func TestDisabledPrefSuppressesRow(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:sub"}})
	prefs := models.DefaultNotificationPreferences(syntax.DID("did:sub"))
	prefs.IssueCreated = false
	if err := deldb.UpsertNotificationPreferences(i.db, prefs); err != nil {
		t.Fatalf("upsert prefs: %v", err)
	}
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil)
	if got := countFor(t, i, "did:sub"); got != 0 {
		t.Fatalf("disabled-pref rows = %d, want 0", got)
	}
}
