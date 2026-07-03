package migration

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/samber/lo"
	cbg "github.com/whyrusleeping/cbor-gen"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func TestBackfillRecordRoundTrip(t *testing.T) {
	createdAt := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	for _, tc := range []struct {
		name           string
		subject        syntax.ATURI
		value          models.StateValue
		wantCollection string
		fromRecord     func(cbg.CBORMarshaler) (models.StateRecord, error)
	}{
		{
			name:           "issue closed",
			subject:        "at://did:plc:boltless/sh.tangled.repo.issue/issue1",
			value:          models.StateClosed,
			wantCollection: tangled.RepoIssueStateNSID,
			fromRecord: func(r cbg.CBORMarshaler) (models.StateRecord, error) {
				return models.IssueStateFromRecord("did:plc:akshay", "s1", *r.(*tangled.RepoIssueState))
			},
		},
		{
			name:           "pull merged",
			subject:        "at://did:plc:boltless/sh.tangled.repo.pull/pull1",
			value:          models.StateMerged,
			wantCollection: tangled.RepoPullStatusNSID,
			fromRecord: func(r cbg.CBORMarshaler) (models.StateRecord, error) {
				return models.PullStatusFromRecord("did:plc:akshay", "s1", *r.(*tangled.RepoPullStatus))
			},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			collection, record, err := backfillRecord(db.BackfillSubject{
				Subject:   tc.subject,
				Value:     tc.value,
				CreatedAt: createdAt.Format(time.RFC3339),
			}, createdAt)
			if err != nil {
				t.Fatalf("backfillRecord: %v", err)
			}
			if collection != tc.wantCollection {
				t.Fatalf("collection = %s, want %s", collection, tc.wantCollection)
			}
			rec, err := tc.fromRecord(record)
			if err != nil {
				t.Fatalf("fromRecord: %v", err)
			}
			if rec.Value != tc.value {
				t.Fatalf("round-trip value = %q, want %q", rec.Value, tc.value)
			}
			if rec.Subject != tc.subject {
				t.Fatalf("round-trip subject = %s, want %s", rec.Subject, tc.subject)
			}
			if rec.SortMicros != createdAt.UnixMicro() {
				t.Fatalf("round-trip sort micros = %d, want %d", rec.SortMicros, createdAt.UnixMicro())
			}
		})
	}
}

func TestBackfillRecordRejects(t *testing.T) {
	for _, tc := range []struct {
		name    string
		subject syntax.ATURI
		value   models.StateValue
	}{
		{"issue subject with merged value", "at://did:plc:boltless/sh.tangled.repo.issue/issue1", models.StateMerged},
		{"non issue or pull subject", "at://did:plc:boltless/sh.tangled.feed.star/x", models.StateClosed},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if _, _, err := backfillRecord(db.BackfillSubject{
				Subject: tc.subject,
				Value:   tc.value,
			}, time.Unix(0, 0)); err == nil {
				t.Fatalf("backfillRecord must reject %s", tc.name)
			}
		})
	}
}

type capturedPut struct {
	Collection string          `json:"collection"`
	Repo       string          `json:"repo"`
	Rkey       string          `json:"rkey"`
	Record     json.RawMessage `json:"record"`
}

type putRecordStore struct {
	mu   sync.Mutex
	puts []capturedPut
}

func (s *putRecordStore) add(p capturedPut) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.puts = append(s.puts, p)
}

func (s *putRecordStore) count() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.puts)
}

func (s *putRecordStore) byRkey() map[string]capturedPut {
	s.mu.Lock()
	defer s.mu.Unlock()
	return lo.SliceToMap(s.puts, func(p capturedPut) (string, capturedPut) {
		return p.Rkey, p
	})
}

func discardLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func newPutRecordServer(t *testing.T, failRkeys map[string]bool) (*putRecordStore, *atclient.APIClient) {
	t.Helper()
	store := &putRecordStore{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasSuffix(r.URL.Path, "/com.atproto.repo.putRecord") {
			t.Errorf("unexpected request path: %s", r.URL.Path)
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		var in capturedPut
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		if failRkeys[in.Rkey] {
			http.Error(w, `{"error":"InternalServerError","message":"boom"}`, http.StatusInternalServerError)
			return
		}
		store.add(in)
		_ = json.NewEncoder(w).Encode(map[string]string{
			"uri": "at://" + in.Repo + "/" + in.Collection + "/" + in.Rkey,
			"cid": "bafyreigxt-test",
		})
	}))
	t.Cleanup(srv.Close)
	return store, &atclient.APIClient{Host: srv.URL, Client: srv.Client()}
}

func seedRepoForOwner(t *testing.T, d *db.DB, owner, repoDid string) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := db.AddRepo(tx, &models.Repo{
		Did:     owner,
		Name:    "anemone",
		Knot:    "knot.example",
		Rkey:    "anemone",
		RepoDid: repoDid,
	}); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}

func seedIssueRow(t *testing.T, d *db.DB, repoDid, author, rkey string) *models.Issue {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	issue := &models.Issue{
		Did:     author,
		Rkey:    rkey,
		RepoDid: syntax.DID(repoDid),
		Title:   "title",
		Body:    "body",
		Open:    true,
	}
	if err := db.PutIssue(tx, issue); err != nil {
		t.Fatalf("PutIssue: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return issue
}

func seedPullRow(t *testing.T, d *db.DB, repoDid, author, rkey string) *models.Pull {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	pull := &models.Pull{
		RepoDid:      syntax.DID(repoDid),
		OwnerDid:     author,
		Rkey:         rkey,
		Title:        "title",
		Body:         "body",
		TargetBranch: "main",
		State:        models.PullOpen,
	}
	if err := db.PutPull(tx, pull); err != nil {
		t.Fatalf("PutPull: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return pull
}

func parseIssueStatePut(did, rkey string, raw json.RawMessage) (models.StateRecord, error) {
	var rec tangled.RepoIssueState
	if err := json.Unmarshal(raw, &rec); err != nil {
		return models.StateRecord{}, err
	}
	return models.IssueStateFromRecord(did, rkey, rec)
}

func parsePullStatusPut(did, rkey string, raw json.RawMessage) (models.StateRecord, error) {
	var rec tangled.RepoPullStatus
	if err := json.Unmarshal(raw, &rec); err != nil {
		return models.StateRecord{}, err
	}
	return models.PullStatusFromRecord(did, rkey, rec)
}

func assertStatePut(
	t *testing.T,
	put capturedPut,
	owner syntax.DID,
	subject syntax.ATURI,
	wantCollection string,
	want models.StateValue,
	parse func(did, rkey string, raw json.RawMessage) (models.StateRecord, error),
) {
	t.Helper()
	if put.Collection != wantCollection {
		t.Fatalf("collection = %q, want %q", put.Collection, wantCollection)
	}
	if put.Repo != owner.String() {
		t.Fatalf("repo = %q, want owner %q", put.Repo, owner)
	}
	if put.Rkey != subject.RecordKey().String() {
		t.Fatalf("rkey = %q, want subject rkey %q", put.Rkey, subject.RecordKey())
	}
	sr, err := parse(owner.String(), put.Rkey, put.Record)
	if err != nil {
		t.Fatalf("parse state record: %v", err)
	}
	if sr.Subject != subject {
		t.Fatalf("subject = %s, want %s", sr.Subject, subject)
	}
	if sr.Value != want {
		t.Fatalf("value = %q, want %q", sr.Value, want)
	}
}

func TestBackfillEntityStateWritesRecords(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	const repoDid = "did:plc:anemone"
	seedRepoForOwner(t, d, string(owner), repoDid)

	closedIssue := seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueClosed")
	seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueOpen")
	mergedPull := seedPullRow(t, d, repoDid, "did:plc:boltless", "pullMerged")
	closedPull := seedPullRow(t, d, repoDid, "did:plc:boltless", "pullClosed")
	seedPullRow(t, d, repoDid, "did:plc:boltless", "pullOpen")

	if err := db.CloseIssues(d, orm.FilterEq("at_uri", closedIssue.AtUri())); err != nil {
		t.Fatalf("CloseIssues: %v", err)
	}
	if err := db.MergePulls(d, orm.FilterEq("at_uri", mergedPull.AtUri())); err != nil {
		t.Fatalf("MergePulls: %v", err)
	}
	if err := db.ClosePulls(d, orm.FilterEq("at_uri", closedPull.AtUri())); err != nil {
		t.Fatalf("ClosePulls: %v", err)
	}

	store, client := newPutRecordServer(t, nil)
	client.AccountDID = &owner
	m := &Migration{db: d, logger: discardLogger()}

	if err := m.backfillEntityState(context.Background(), client, owner, ""); err != nil {
		t.Fatalf("backfillEntityState: %v", err)
	}

	got := store.byRkey()
	if len(got) != 3 {
		t.Fatalf("wrote %d records, want 3: %+v", len(got), got)
	}
	assertStatePut(t, got["issueClosed"], owner, closedIssue.AtUri(), tangled.RepoIssueStateNSID, models.StateClosed, parseIssueStatePut)
	assertStatePut(t, got["pullMerged"], owner, mergedPull.AtUri(), tangled.RepoPullStatusNSID, models.StateMerged, parsePullStatusPut)
	assertStatePut(t, got["pullClosed"], owner, closedPull.AtUri(), tangled.RepoPullStatusNSID, models.StateClosed, parsePullStatusPut)
}

func TestBackfillEntityStateSkipsCleanOwner(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	const repoDid = "did:plc:anemone"
	seedRepoForOwner(t, d, string(owner), repoDid)
	seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueOpen")

	store, client := newPutRecordServer(t, nil)
	m := &Migration{db: d, logger: discardLogger()}

	if err := m.backfillEntityState(context.Background(), client, owner, ""); err != nil {
		t.Fatalf("backfillEntityState: %v", err)
	}
	if store.count() != 0 {
		t.Fatalf("wrote %d records for an owner with no column-only closed state, want 0", store.count())
	}
}

func TestBackfillEntityStateCreatedAt(t *testing.T) {
	created := time.Date(2024, 3, 4, 5, 6, 7, 0, time.UTC)
	for _, tc := range []struct {
		name   string
		stored string
		want   time.Time
	}{
		{"uses subject created", created.Format(time.RFC3339), created},
		{"falls back to epoch on unparseable", "not-a-timestamp", time.Unix(0, 0).UTC()},
	} {
		t.Run(tc.name, func(t *testing.T) {
			d := newTestDB(t)
			owner := syntax.DID("did:plc:akshay")
			const repoDid = "did:plc:anemone"
			seedRepoForOwner(t, d, string(owner), repoDid)
			issue := seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueClosed")
			if err := db.CloseIssues(d, orm.FilterEq("at_uri", issue.AtUri())); err != nil {
				t.Fatalf("CloseIssues: %v", err)
			}
			if _, err := d.Exec(`update issues set created = ? where at_uri = ?`, tc.stored, issue.AtUri()); err != nil {
				t.Fatalf("update created: %v", err)
			}

			store, client := newPutRecordServer(t, nil)
			client.AccountDID = &owner
			m := &Migration{db: d, logger: discardLogger()}

			if err := m.backfillEntityState(context.Background(), client, owner, ""); err != nil {
				t.Fatalf("backfillEntityState: %v", err)
			}

			var rec tangled.RepoIssueState
			if err := json.Unmarshal(store.byRkey()["issueClosed"].Record, &rec); err != nil {
				t.Fatalf("unmarshal: %v", err)
			}
			want, err := models.AsIssueStateRecord(issue.AtUri(), models.StateClosed, tc.want)
			if err != nil {
				t.Fatalf("AsIssueStateRecord: %v", err)
			}
			if rec.CreatedAt != want.CreatedAt {
				t.Fatalf("createdAt = %q, want %q", rec.CreatedAt, want.CreatedAt)
			}
		})
	}
}

func TestBackfillEntityStatePartialFailureReturnsError(t *testing.T) {
	d := newTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	const repoDid = "did:plc:anemone"
	seedRepoForOwner(t, d, string(owner), repoDid)
	good := seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueGood")
	bad := seedIssueRow(t, d, repoDid, "did:plc:boltless", "issueBad")
	if err := db.CloseIssues(d, orm.FilterEq("at_uri", good.AtUri())); err != nil {
		t.Fatalf("CloseIssues good: %v", err)
	}
	if err := db.CloseIssues(d, orm.FilterEq("at_uri", bad.AtUri())); err != nil {
		t.Fatalf("CloseIssues bad: %v", err)
	}

	store, client := newPutRecordServer(t, map[string]bool{"issueBad": true})
	client.AccountDID = &owner
	m := &Migration{db: d, logger: discardLogger()}

	if err := m.backfillEntityState(context.Background(), client, owner, ""); err == nil {
		t.Fatal("backfillEntityState must return an error when a subject write fails")
	}

	got := store.byRkey()
	if _, ok := got["issueGood"]; !ok {
		t.Fatal("the succeeding subject must still be written when another fails")
	}
	if _, ok := got["issueBad"]; ok {
		t.Fatal("the failing subject must not be recorded as written")
	}
}
