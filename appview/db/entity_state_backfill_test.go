package db

import (
	"context"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/samber/lo"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func TestColumnOnlyClosedSubjectsForOwner(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	repo := seedRepo(t, d, string(owner), "knot.example", "anemone", "anemone", "did:plc:anemone")

	closedIssue := seedIssue(t, d, repo, "did:plc:boltless", "issueClosed")
	seedIssue(t, d, repo, "did:plc:boltless", "issueOpen")
	recordedIssue := seedIssue(t, d, repo, "did:plc:boltless", "issueRecorded")

	if err := CloseIssues(d, orm.FilterEq("at_uri", closedIssue.AtUri())); err != nil {
		t.Fatalf("CloseIssues closed: %v", err)
	}
	if err := CloseIssues(d, orm.FilterEq("at_uri", recordedIssue.AtUri())); err != nil {
		t.Fatalf("CloseIssues recorded: %v", err)
	}
	putIssueStateRec(t, d, issueRec("did:plc:boltless", "srec", recordedIssue.AtUri(), models.StateClosed, 100))

	emptyRkey := seedIssue(t, d, repo, "did:plc:boltless", "")
	if err := CloseIssues(d, orm.FilterEq("at_uri", emptyRkey.AtUri())); err != nil {
		t.Fatalf("CloseIssues empty rkey: %v", err)
	}

	mergedPull := seedPull(t, d, repo, "did:plc:boltless", "pullMerged")
	closedPull := seedPull(t, d, repo, "did:plc:boltless", "pullClosed")
	seedPull(t, d, repo, "did:plc:boltless", "pullOpen")

	if err := MergePulls(d, orm.FilterEq("at_uri", mergedPull.AtUri())); err != nil {
		t.Fatalf("MergePulls: %v", err)
	}
	if err := ClosePulls(d, orm.FilterEq("at_uri", closedPull.AtUri())); err != nil {
		t.Fatalf("ClosePulls: %v", err)
	}

	subjects, err := ColumnOnlyClosedSubjectsForOwner(context.Background(), d, owner)
	if err != nil {
		t.Fatalf("ColumnOnlyClosedSubjectsForOwner: %v", err)
	}

	got := lo.SliceToMap(subjects, func(s BackfillSubject) (syntax.ATURI, models.StateValue) {
		return s.Subject, s.Value
	})

	if len(got) != 3 {
		t.Fatalf("got %d subjects, want 3: %+v", len(got), got)
	}
	if got[closedIssue.AtUri()] != models.StateClosed {
		t.Fatalf("closed issue: got %q want closed", got[closedIssue.AtUri()])
	}
	if got[mergedPull.AtUri()] != models.StateMerged {
		t.Fatalf("merged pull: got %q want merged", got[mergedPull.AtUri()])
	}
	if got[closedPull.AtUri()] != models.StateClosed {
		t.Fatalf("closed pull: got %q want closed", got[closedPull.AtUri()])
	}
	if _, ok := got[emptyRkey.AtUri()]; ok {
		t.Fatal("closed issue with empty rkey must be excluded")
	}
}

func mustEnqueueBackfill(t *testing.T, d *DB) {
	t.Helper()
	if _, err := EnqueueEntityStateBackfill(context.Background(), d); err != nil {
		t.Fatalf("EnqueueEntityStateBackfill: %v", err)
	}
}

func markBackfillDone(t *testing.T, d *DB) {
	t.Helper()
	if _, err := d.Exec(
		`update pds_migration set status = 'done' where name = ?`, EntityStateBackfillName,
	); err != nil {
		t.Fatalf("mark done: %v", err)
	}
}

func TestEnqueueEntityStateBackfill(t *testing.T) {
	owner := syntax.DID("did:plc:akshay")
	for _, tc := range []struct {
		name       string
		arrange    func(t *testing.T, d *DB, issue *models.Issue)
		wantRows   int64
		wantNoRow  bool
		wantStatus models.PDSMigrationStatus
	}{
		{
			name:       "enqueues owner with column-only closed work",
			arrange:    func(t *testing.T, d *DB, issue *models.Issue) {},
			wantRows:   1,
			wantStatus: models.PDSMigrationStatusPending,
		},
		{
			name:       "idempotent while still pending",
			arrange:    func(t *testing.T, d *DB, issue *models.Issue) { mustEnqueueBackfill(t, d) },
			wantRows:   0,
			wantStatus: models.PDSMigrationStatusPending,
		},
		{
			name: "re-arms completed owner with fresh work",
			arrange: func(t *testing.T, d *DB, issue *models.Issue) {
				mustEnqueueBackfill(t, d)
				markBackfillDone(t, d)
			},
			wantRows:   1,
			wantStatus: models.PDSMigrationStatusPending,
		},
		{
			name: "leaves settled owner done",
			arrange: func(t *testing.T, d *DB, issue *models.Issue) {
				mustEnqueueBackfill(t, d)
				putIssueStateRec(t, d, issueRec(string(owner), "srec", issue.AtUri(), models.StateClosed, 100))
				markBackfillDone(t, d)
			},
			wantRows:   0,
			wantStatus: models.PDSMigrationStatusDone,
		},
		{
			name: "skips owner whose closed work is already recorded",
			arrange: func(t *testing.T, d *DB, issue *models.Issue) {
				putIssueStateRec(t, d, issueRec(string(owner), "srec", issue.AtUri(), models.StateClosed, 100))
			},
			wantRows:  0,
			wantNoRow: true,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			d := newTestDB(t)
			repo := seedRepo(t, d, string(owner), "knot.example", "anemone", "anemone", "did:plc:anemone")
			issue := seedIssue(t, d, repo, "did:plc:boltless", "issue1")
			if err := CloseIssues(d, orm.FilterEq("at_uri", issue.AtUri())); err != nil {
				t.Fatalf("CloseIssues: %v", err)
			}
			tc.arrange(t, d, issue)

			n, err := EnqueueEntityStateBackfill(context.Background(), d)
			if err != nil {
				t.Fatalf("EnqueueEntityStateBackfill: %v", err)
			}
			if n != tc.wantRows {
				t.Fatalf("enqueue affected %d rows, want %d", n, tc.wantRows)
			}

			if tc.wantNoRow {
				var count int
				if err := d.QueryRow(
					`select count(*) from pds_migration where name = ?`, EntityStateBackfillName,
				).Scan(&count); err != nil {
					t.Fatalf("query: %v", err)
				}
				if count != 0 {
					t.Fatalf("owner with no column-only closed work produced %d rows, want 0", count)
				}
				return
			}

			var did, status string
			if err := d.QueryRow(
				`select did, status from pds_migration where name = ?`, EntityStateBackfillName,
			).Scan(&did, &status); err != nil {
				t.Fatalf("query: %v", err)
			}
			if syntax.DID(did) != owner {
				t.Fatalf("row did = %s, want owner %s", did, owner)
			}
			if status != string(tc.wantStatus) {
				t.Fatalf("row status = %s, want %s", status, tc.wantStatus)
			}
		})
	}
}
