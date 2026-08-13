package deliberi

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"path/filepath"
	"testing"

	comatprototypes "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	deldb "tangled.org/core/deliberi/db"
	models "tangled.org/core/deliberi/models"
)

type fakeResolver struct {
	dids []string
	// bySubject, when set, answers per subject instead of unconditionally.
	bySubject map[string][]string
	err       error

	// repo owner lookups, keyed by repo did.
	owners    map[string]string
	repoNames map[string]string
	ownerErr  error
	// ownerCalls counts RepoOwner calls, to prove cache hits skip the network.
	ownerCalls *int
}

func (f fakeResolver) ListRecipients(ctx context.Context, uri string, collection string) ([]string, error) {
	if f.bySubject != nil {
		return f.bySubject[uri], f.err
	}
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
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil, "")
	if got := countFor(t, i, "did:sub"); got != 1 {
		t.Fatalf("subscriber rows = %d, want 1", got)
	}
}

func TestActorNeverNotified(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:actor"}})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", []string{"did:actor"}, "")
	if got := countFor(t, i, "did:actor"); got != 0 {
		t.Fatalf("actor rows = %d, want 0", got)
	}
}

func TestMentionDeliveredOnResolverError(t *testing.T) {
	i := newTestIngester(t, fakeResolver{err: io.ErrUnexpectedEOF})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", []string{"did:mention"}, "")
	if got := countFor(t, i, "did:mention"); got != 1 {
		t.Fatalf("mention rows = %d, want 1", got)
	}
}

func TestCreateNotificationDedupe(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:sub"}})
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil, "")
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil, "")
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

func TestCommentNotifiesRepoSubscriber(t *testing.T) {
	const (
		repoDid  = "did:plc:therepo"
		repoName = "my-repo"
		issueUri = "at://did:plc:bob/sh.tangled.repo.issue/issue1"
	)

	// subscribed to the repo, not the issue, so a row proves the repo lookup ran.
	i := newTestIngester(t, fakeResolver{bySubject: map[string][]string{
		repoDid: {"did:sub"},
	}})

	if err := deldb.PutRepoName(i.db, repoDid, "did:plc:bob", repoName); err != nil {
		t.Fatalf("PutRepoName: %v", err)
	}
	if err := deldb.PutEntityTitle(i.db, issueUri, "the issue", repoDid); err != nil {
		t.Fatalf("PutEntityTitle: %v", err)
	}

	raw, err := json.Marshal(tangled.FeedComment{
		CreatedAt: "2026-01-01T00:00:00Z",
		Subject:   &comatprototypes.RepoStrongRef{Uri: issueUri, Cid: "bafyfake"},
	})
	if err != nil {
		t.Fatalf("marshal comment: %v", err)
	}
	ev := &jmodels.Event{
		Did:  "did:plc:alice",
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationCreate,
			Collection: tangled.FeedCommentNSID,
			RKey:       "comment1",
			Record:     raw,
		},
	}
	if err := i.process(context.Background(), ev); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := countFor(t, i, "did:sub"); got != 1 {
		t.Fatalf("repo subscriber rows = %d, want 1", got)
	}

	var gotRepoDid, gotTitle string
	err = i.db.QueryRow(
		`select repo_did, entity_title from notifications where recipient_did = ?`,
		"did:sub",
	).Scan(&gotRepoDid, &gotTitle)
	if err != nil {
		t.Fatalf("scan notification: %v", err)
	}
	if gotRepoDid != repoDid {
		t.Errorf("repo_did = %q, want %q", gotRepoDid, repoDid)
	}
	if gotTitle != "the issue" {
		t.Errorf("entity_title = %q, want %q", gotTitle, "the issue")
	}
}

func TestCommentNotifiesEntitySubscriber(t *testing.T) {
	const (
		repoDid  = "did:plc:therepo"
		issueUri = "at://did:plc:bob/sh.tangled.repo.issue/issue1"
	)

	i := newTestIngester(t, fakeResolver{bySubject: map[string][]string{
		issueUri: {"did:entitysub"},
	}})
	if err := deldb.PutEntityTitle(i.db, issueUri, "the issue", repoDid); err != nil {
		t.Fatalf("PutEntityTitle: %v", err)
	}

	raw, err := json.Marshal(tangled.FeedComment{
		CreatedAt: "2026-01-01T00:00:00Z",
		Subject:   &comatprototypes.RepoStrongRef{Uri: issueUri, Cid: "bafyfake"},
	})
	if err != nil {
		t.Fatalf("marshal comment: %v", err)
	}
	ev := &jmodels.Event{
		Did:  "did:plc:alice",
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationCreate,
			Collection: tangled.FeedCommentNSID,
			RKey:       "comment1",
			Record:     raw,
		},
	}
	if err := i.process(context.Background(), ev); err != nil {
		t.Fatalf("process: %v", err)
	}

	if got := countFor(t, i, "did:entitysub"); got != 1 {
		t.Fatalf("entity subscriber rows = %d, want 1", got)
	}
}

func TestPutEntityTitleKeepsRepoDid(t *testing.T) {
	database, err := deldb.Make(context.Background(), filepath.Join(t.TempDir(), "x.db"))
	if err != nil {
		t.Fatalf("make db: %v", err)
	}
	t.Cleanup(func() { database.Close() })

	const uri = "at://did:plc:bob/sh.tangled.repo.issue/issue1"
	if err := deldb.PutEntityTitle(database, uri, "t1", "did:plc:therepo"); err != nil {
		t.Fatalf("first put: %v", err)
	}
	// a later write that does not know the repo must not erase it.
	if err := deldb.PutEntityTitle(database, uri, "t2", ""); err != nil {
		t.Fatalf("second put: %v", err)
	}
	if got := deldb.GetEntityRepo(database, uri); got != "did:plc:therepo" {
		t.Errorf("repo did = %q, want it preserved", got)
	}
	if got := deldb.GetEntityTitle(database, uri); got != "t2" {
		t.Errorf("title = %q, want %q", got, "t2")
	}
}

func TestDisabledPrefSuppressesRow(t *testing.T) {
	i := newTestIngester(t, fakeResolver{dids: []string{"did:sub"}})
	prefs := models.DefaultNotificationPreferences(syntax.DID("did:sub"))
	prefs.IssueCreated = false
	if err := deldb.UpsertNotificationPreferences(i.db, prefs); err != nil {
		t.Fatalf("upsert prefs: %v", err)
	}
	i.notifyEntity(context.Background(), "did:actor", "at://src", "at://entity", "did:repo", models.NotificationTypeIssueCreated, "title", nil, "")
	if got := countFor(t, i, "did:sub"); got != 0 {
		t.Fatalf("disabled-pref rows = %d, want 0", got)
	}
}
