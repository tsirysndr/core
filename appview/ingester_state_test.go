package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func newStateIngester(t *testing.T) *Ingester {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := db.Make(t.Context(), path)
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return &Ingester{
		Ctx:    t.Context(),
		Db:     d,
		Logger: slog.New(slog.DiscardHandler),
	}
}

type stubAcl struct {
	allow bool
	err   error
}

func (s stubAcl) HasRepoPermissionErr(ctx context.Context, repo *models.Repo, userDid, perm string) (bool, error) {
	return s.allow, s.err
}

func seedTx(t *testing.T, d *db.DB, fn func(tx *sql.Tx) error) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		t.Fatalf("seed: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}

func seedRepo(t *testing.T, d *db.DB, ownerDid, repoDid string) {
	t.Helper()
	seedTx(t, d, func(tx *sql.Tx) error {
		return db.AddRepo(tx, &models.Repo{
			Did:     ownerDid,
			Name:    "anemone",
			Knot:    "knot.example",
			Rkey:    "anemone",
			RepoDid: repoDid,
		})
	})
}

func seedIssue(t *testing.T, d *db.DB, ownerDid, repoDid, issueRkey string) syntax.ATURI {
	t.Helper()
	issue := &models.Issue{
		Did:     ownerDid,
		Rkey:    issueRkey,
		RepoDid: syntax.DID(repoDid),
		Title:   "title",
		Body:    "body",
		Open:    true,
	}
	seedTx(t, d, func(tx *sql.Tx) error {
		return db.PutIssue(tx, issue)
	})
	return issue.AtUri()
}

func seedRepoAndIssue(t *testing.T, d *db.DB, ownerDid, repoDid, issueRkey string) syntax.ATURI {
	t.Helper()
	seedRepo(t, d, ownerDid, repoDid)
	return seedIssue(t, d, ownerDid, repoDid, issueRkey)
}

func issueStateEvent(t *testing.T, op, did, rkey, subject, state, createdAt string) *jmodels.Event {
	t.Helper()
	raw, err := json.Marshal(tangled.RepoIssueState{
		Issue:     subject,
		State:     state,
		CreatedAt: createdAt,
	})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return &jmodels.Event{
		Did:  did,
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  op,
			Collection: tangled.RepoIssueStateNSID,
			RKey:       rkey,
			Record:     raw,
		},
	}
}

func ingestedIssueOpen(t *testing.T, d *db.DB, subject syntax.ATURI) bool {
	t.Helper()
	issues, err := db.GetIssues(d, orm.FilterEq("at_uri", subject))
	if err != nil || len(issues) != 1 {
		t.Fatalf("GetIssues: %v len %d", err, len(issues))
	}
	return issues[0].Open
}

func TestIngestState_Authorization(t *testing.T) {
	owner := "did:plc:boltless"
	cases := []struct {
		name     string
		author   string
		acl      RepoPermissionChecker
		wantOpen bool
	}{
		{"author closes own issue", owner, stubAcl{err: errors.New("acl must not be consulted for the author")}, false},
		{"collaborator with push closes", "did:plc:squid", stubAcl{allow: true}, false},
		{"stranger without push rejected", "did:plc:squid", stubAcl{allow: false}, true},
		{"unreachable knot fails open", "did:plc:squid", stubAcl{allow: false, err: knotacl.ErrKnotUnreachable}, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ing := newStateIngester(t)
			ing.Acl = tc.acl
			at := seedRepoAndIssue(t, ing.Db, owner, "did:plc:anemone", "issue1")

			ev := issueStateEvent(t, jmodels.CommitOperationCreate, tc.author, "s1", string(at), tangled.RepoIssueStateClosed, "2026-06-01T00:00:00Z")
			if err := ing.ingestState(context.Background(), ev, ing.Logger, issueStateSpec); err != nil {
				t.Fatalf("ingestState: %v", err)
			}

			if got := ingestedIssueOpen(t, ing.Db, at); got != tc.wantOpen {
				t.Fatalf("issue open = %v, want %v", got, tc.wantOpen)
			}
			if pending, _ := db.PendingStateRecordsForSubject(ing.Db, at); len(pending) != 0 {
				t.Fatalf("a record whose subject exists must resolve, not park: pending=%d", len(pending))
			}
		})
	}
}

func TestIngestState_ParkThenReconcileDrains(t *testing.T) {
	ing := newStateIngester(t)
	owner := "did:plc:boltless"
	subject := syntax.ATURI("at://" + owner + "/" + tangled.RepoIssueNSID + "/issue1")

	ev := issueStateEvent(t, jmodels.CommitOperationCreate, owner, "s1", string(subject), tangled.RepoIssueStateClosed, "2026-06-01T00:00:00Z")
	if err := ing.ingestState(ing.Ctx, ev, ing.Logger, issueStateSpec); err != nil {
		t.Fatalf("park: %v", err)
	}
	if pending, _ := db.PendingStateRecordsForSubject(ing.Db, subject); len(pending) != 1 {
		t.Fatalf("a state record whose subject is missing must be parked, pending=%d", len(pending))
	}

	at := seedRepoAndIssue(t, ing.Db, owner, "did:plc:anemone", "issue1")
	if at != subject {
		t.Fatalf("subject mismatch: %s vs %s", at, subject)
	}
	ing.ReconcilePendingState()

	if ingestedIssueOpen(t, ing.Db, at) {
		t.Fatal("the reconciler must drain a parked record once its subject exists")
	}
	if pending, _ := db.PendingStateRecordsForSubject(ing.Db, at); len(pending) != 0 {
		t.Fatalf("a drained record must be unparked, pending=%d", len(pending))
	}
}

func TestIngestState_DeleteUnparksBeforeSubjectArrives(t *testing.T) {
	ing := newStateIngester(t)
	ctx := context.Background()
	owner := "did:plc:boltless"
	subject := syntax.ATURI("at://" + owner + "/" + tangled.RepoIssueNSID + "/issue1")

	create := issueStateEvent(t, jmodels.CommitOperationCreate, owner, "s1", string(subject), tangled.RepoIssueStateClosed, "2026-06-01T00:00:00Z")
	if err := ing.ingestState(ctx, create, ing.Logger, issueStateSpec); err != nil {
		t.Fatalf("park: %v", err)
	}

	del := &jmodels.Event{
		Did:  owner,
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationDelete,
			Collection: tangled.RepoIssueStateNSID,
			RKey:       "s1",
		},
	}
	if err := ing.ingestState(ctx, del, ing.Logger, issueStateSpec); err != nil {
		t.Fatalf("delete: %v", err)
	}

	if pending, _ := db.PendingStateRecordsForSubject(ing.Db, subject); len(pending) != 0 {
		t.Fatalf("deleting a parked record must unpark it, pending=%d", len(pending))
	}

	at := seedRepoAndIssue(t, ing.Db, owner, "did:plc:anemone", "issue1")
	ing.drainPendingState(ctx, at, ing.Logger)
	if !ingestedIssueOpen(t, ing.Db, at) {
		t.Fatal("a deleted parked record must not apply after the subject arrives")
	}
}

func TestIngestState_ParkedUnauthorizedDroppedOnDrain(t *testing.T) {
	ing := newStateIngester(t)
	ing.Acl = stubAcl{allow: false}
	ctx := context.Background()
	owner := "did:plc:boltless"
	subject := syntax.ATURI("at://" + owner + "/" + tangled.RepoIssueNSID + "/issue1")

	ev := issueStateEvent(t, jmodels.CommitOperationCreate, "did:plc:squid", "s1", string(subject), tangled.RepoIssueStateClosed, "2026-06-01T00:00:00Z")
	if err := ing.ingestState(ctx, ev, ing.Logger, issueStateSpec); err != nil {
		t.Fatalf("park: %v", err)
	}
	if pending, _ := db.PendingStateRecordsForSubject(ing.Db, subject); len(pending) != 1 {
		t.Fatalf("a record whose subject is missing parks before any auth check: pending=%d", len(pending))
	}

	at := seedRepoAndIssue(t, ing.Db, owner, "did:plc:anemone", "issue1")
	ing.drainPendingState(ctx, at, ing.Logger)

	if !ingestedIssueOpen(t, ing.Db, at) {
		t.Fatal("a parked record that fails authorization on drain must not apply")
	}
	if pending, _ := db.PendingStateRecordsForSubject(ing.Db, at); len(pending) != 0 {
		t.Fatalf("a parked record rejected on drain must be unparked, not left to re-accumulate: pending=%d", len(pending))
	}
}
