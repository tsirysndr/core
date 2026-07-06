package db

import (
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func seedIssue(t *testing.T, d *DB, repo *models.Repo, did, rkey string) *models.Issue {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	issue := &models.Issue{
		Did:     did,
		Rkey:    rkey,
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
	return issue
}

func seedPull(t *testing.T, d *DB, repo *models.Repo, did, rkey string) *models.Pull {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	pull := &models.Pull{
		RepoDid:      syntax.DID(repo.RepoDid),
		OwnerDid:     did,
		Rkey:         rkey,
		Title:        "title",
		Body:         "body",
		TargetBranch: "main",
		State:        models.PullOpen,
	}
	if err := PutPull(tx, pull); err != nil {
		t.Fatalf("PutPull: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return pull
}

func putIssueStateRec(t *testing.T, d *DB, rec models.StateRecord) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if _, err := PutIssueState(tx, rec); err != nil {
		t.Fatalf("PutIssueState: %v", err)
	}
	if err := ResolveIssueState(tx, rec.Subject); err != nil {
		t.Fatalf("ResolveIssueState: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}

func deleteIssueStateRec(t *testing.T, d *DB, did, rkey string) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	subject, err := DeleteIssueState(tx, did, rkey)
	if err != nil {
		t.Fatalf("DeleteIssueState: %v", err)
	}
	if subject != "" {
		if err := RecomputeIssueState(tx, subject); err != nil {
			t.Fatalf("RecomputeIssueState: %v", err)
		}
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}

func putPullStatusRec(t *testing.T, d *DB, rec models.StateRecord) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if _, err := PutPullStatus(tx, rec); err != nil {
		t.Fatalf("PutPullStatus: %v", err)
	}
	if err := ResolvePullStatus(tx, rec.Subject); err != nil {
		t.Fatalf("ResolvePullStatus: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
}

func issueOpen(t *testing.T, d *DB, subject syntax.ATURI) bool {
	t.Helper()
	issues, err := GetIssues(d, orm.FilterEq("at_uri", subject))
	if err != nil || len(issues) != 1 {
		t.Fatalf("GetIssues: %v len %d", err, len(issues))
	}
	return issues[0].Open
}

func pullStateOf(t *testing.T, d *DB, subject syntax.ATURI) models.PullState {
	t.Helper()
	pulls, err := GetPulls(d, orm.FilterEq("at_uri", subject))
	if err != nil || len(pulls) != 1 {
		t.Fatalf("GetPulls: %v len %d", err, len(pulls))
	}
	return pulls[0].State
}

func issueRec(did, rkey string, subject syntax.ATURI, v models.StateValue, micros int64) models.StateRecord {
	return models.StateRecord{Did: did, Rkey: rkey, Subject: subject, Value: v, SortMicros: micros}
}

func TestIssueStateLastWriterWins(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")
	issue := seedIssue(t, d, repo, "did:plc:akshay", "issue1")
	subject := issue.AtUri()

	if !issueOpen(t, d, subject) {
		t.Fatal("new issue should be open")
	}

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s2", subject, models.StateClosed, 200))
	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s1", subject, models.StateOpen, 100))
	if issueOpen(t, d, subject) {
		t.Fatal("earlier open@100 must not beat closed@200")
	}

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s3", subject, models.StateOpen, 300))
	if !issueOpen(t, d, subject) {
		t.Fatal("open@300 should win")
	}

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "zzz", subject, models.StateClosed, 300))
	if issueOpen(t, d, subject) {
		t.Fatal("a tie at 300 must break to the greater source uri zzz=closed")
	}

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "zzz", subject, models.StateClosed, 300))
	if issueOpen(t, d, subject) {
		t.Fatal("replaying the winning record must not change the result")
	}
	var count int
	if err := d.QueryRow(`select count(*) from issue_states where subject = ?`, subject).Scan(&count); err != nil {
		t.Fatalf("count: %v", err)
	}
	if count != 4 {
		t.Fatalf("replaying an existing record must not duplicate rows, got %d want 4", count)
	}
}

func TestIssueStateOrderIndependent(t *testing.T) {
	build := func(order []int) bool {
		d := newTestDB(t)
		repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")
		issue := seedIssue(t, d, repo, "did:plc:akshay", "issue1")
		subject := issue.AtUri()

		recs := []models.StateRecord{
			issueRec("did:plc:akshay", "a", subject, models.StateOpen, 100),
			issueRec("did:plc:akshay", "b", subject, models.StateClosed, 300),
			issueRec("did:plc:akshay", "c", subject, models.StateOpen, 200),
		}
		for _, idx := range order {
			putIssueStateRec(t, d, recs[idx])
		}
		return issueOpen(t, d, subject)
	}

	forward := build([]int{0, 1, 2})
	shuffled := build([]int{2, 0, 1})
	if forward != shuffled {
		t.Fatalf("order changed result: forward=%v shuffled=%v", forward, shuffled)
	}
	if forward {
		t.Fatal("highest-micros record closed@300 must win regardless of order")
	}
}

func TestIssueStateDeleteRecomputes(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")
	issue := seedIssue(t, d, repo, "did:plc:akshay", "issue1")
	subject := issue.AtUri()

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s1", subject, models.StateClosed, 100))
	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s2", subject, models.StateOpen, 200))
	if !issueOpen(t, d, subject) {
		t.Fatal("open@200 should win before deletion")
	}

	deleteIssueStateRec(t, d, "did:plc:akshay", "s2")
	if issueOpen(t, d, subject) {
		t.Fatal("deleting open@200 must fall back to closed@100")
	}

	deleteIssueStateRec(t, d, "did:plc:akshay", "s1")
	if !issueOpen(t, d, subject) {
		t.Fatal("deleting the last state record must revert to open")
	}
}

func TestIssueStateSubjectChangeRecomputesPrior(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")
	issueA := seedIssue(t, d, repo, "did:plc:akshay", "issueA")
	issueB := seedIssue(t, d, repo, "did:plc:akshay", "issueB")

	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s1", issueA.AtUri(), models.StateClosed, 100))
	if issueOpen(t, d, issueA.AtUri()) {
		t.Fatal("issueA should be closed after closed@100")
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	prior, err := PutIssueState(tx, issueRec("did:plc:akshay", "s1", issueB.AtUri(), models.StateClosed, 200))
	if err != nil {
		t.Fatalf("PutIssueState: %v", err)
	}
	if prior != issueA.AtUri() {
		t.Fatalf("put must report the prior subject %s, got %q", issueA.AtUri(), prior)
	}
	if err := ResolveIssueState(tx, issueB.AtUri()); err != nil {
		t.Fatalf("ResolveIssueState B: %v", err)
	}
	if err := RecomputeIssueState(tx, prior); err != nil {
		t.Fatalf("RecomputeIssueState A: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	if !issueOpen(t, d, issueA.AtUri()) {
		t.Fatal("issueA must revert to open once its only state record repoints to issueB")
	}
	if issueOpen(t, d, issueB.AtUri()) {
		t.Fatal("issueB should be closed after the record repoints to it")
	}
}

func TestPullStatusLastWriterWins(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "limpet", "limpet", "did:plc:limpet")
	pull := seedPull(t, d, repo, "did:plc:akshay", "pull1")
	subject := pull.AtUri()

	if pullStateOf(t, d, subject) != models.PullOpen {
		t.Fatal("new pull should be open")
	}

	putPullStatusRec(t, d, issueRec("did:plc:akshay", "s1", subject, models.StateClosed, 100))
	if pullStateOf(t, d, subject) != models.PullClosed {
		t.Fatal("closed@100 should win")
	}

	putPullStatusRec(t, d, issueRec("did:plc:akshay", "s2", subject, models.StateMerged, 200))
	if pullStateOf(t, d, subject) != models.PullMerged {
		t.Fatal("merged@200 should win over closed@100")
	}

	putPullStatusRec(t, d, issueRec("did:plc:akshay", "s3", subject, models.StateOpen, 300))
	if pullStateOf(t, d, subject) != models.PullOpen {
		t.Fatal("open@300 should win over merged@200")
	}

	if err := AbandonPulls(d, orm.FilterEq("at_uri", subject)); err != nil {
		t.Fatalf("AbandonPulls: %v", err)
	}
	putPullStatusRec(t, d, issueRec("did:plc:akshay", "s4", subject, models.StateOpen, 400))
	if pullStateOf(t, d, subject) != models.PullAbandoned {
		t.Fatal("an abandoned pull must not be resurrected by a later status record")
	}
}

func TestIssueStateForeignKey(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	ghost := syntax.ATURI("at://did:plc:akshay/sh.tangled.repo.issue/ghost")
	if _, err := PutIssueState(tx, issueRec("did:plc:akshay", "s1", ghost, models.StateClosed, 100)); err == nil {
		t.Fatal("inserting state for a nonexistent issue must violate the foreign key")
	}
	tx.Rollback()

	issue := seedIssue(t, d, repo, "did:plc:akshay", "issue1")
	subject := issue.AtUri()
	putIssueStateRec(t, d, issueRec("did:plc:akshay", "s1", subject, models.StateClosed, 100))

	dtx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := DeleteIssues(dtx, "did:plc:akshay", "issue1"); err != nil {
		t.Fatalf("DeleteIssues: %v", err)
	}
	if err := dtx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	var remaining int
	if err := d.QueryRow(`select count(*) from issue_states where subject = ?`, subject).Scan(&remaining); err != nil {
		t.Fatalf("count: %v", err)
	}
	if remaining != 0 {
		t.Fatalf("deleting the issue must cascade-delete its state rows, got %d", remaining)
	}
}

func TestPendingStateRecords(t *testing.T) {
	d := newTestDB(t)
	s1 := syntax.ATURI("at://did:plc:boltless/sh.tangled.repo.issue/i1")
	s2 := syntax.ATURI("at://did:plc:akshay/sh.tangled.repo.pull/p1")
	issueNsid := "sh.tangled.repo.issue.state"

	park := func(did, rkey, nsid string, subject syntax.ATURI, record string) {
		t.Helper()
		tx, err := d.Begin()
		if err != nil {
			t.Fatalf("Begin: %v", err)
		}
		if err := ParkStateRecord(tx, PendingStateRecord{
			Did: did, Rkey: rkey, Nsid: nsid, Subject: subject, Record: []byte(record),
		}); err != nil {
			t.Fatalf("ParkStateRecord: %v", err)
		}
		if err := tx.Commit(); err != nil {
			t.Fatalf("Commit: %v", err)
		}
	}

	park("did:plc:boltless", "s1", issueNsid, s1, `{"v":1}`)
	park("did:plc:boltless", "s1", issueNsid, s1, `{"v":2}`)
	park("did:plc:akshay", "p1", "sh.tangled.repo.pull.status", s2, `{}`)

	pending, err := PendingStateRecordsForSubject(d, s1)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	if len(pending) != 1 || pending[0].Did != "did:plc:boltless" || string(pending[0].Record) != `{"v":2}` {
		t.Fatalf("re-park must overwrite without duplicating, got %+v", pending)
	}

	subjects, err := DistinctPendingStateSubjects(d)
	if err != nil {
		t.Fatalf("DistinctPendingStateSubjects: %v", err)
	}
	if len(subjects) != 2 {
		t.Fatalf("want 2 distinct subjects from 3 parked rows, got %d", len(subjects))
	}

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := UnparkStateRecord(tx, "did:plc:boltless", "s1", issueNsid); err != nil {
		t.Fatalf("UnparkStateRecord: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	if pending, _ := PendingStateRecordsForSubject(d, s1); len(pending) != 0 {
		t.Fatalf("want 0 after unpark, got %d", len(pending))
	}
}

func TestEvictStalePendingStateRecords(t *testing.T) {
	d := newTestDB(t)
	repo := seedRepo(t, d, "did:plc:akshay", "knot.example", "anemone", "anemone", "did:plc:anemone")
	issue := seedIssue(t, d, repo, "did:plc:akshay", "issue1")
	live := issue.AtUri()
	orphan := syntax.ATURI("at://did:plc:boltless/sh.tangled.repo.issue/ghost")

	insert := func(rkey, created string, subject syntax.ATURI) {
		t.Helper()
		if _, err := d.Exec(
			`insert into pending_state_records (did, rkey, nsid, subject, record, created) values (?, ?, ?, ?, ?, ?)`,
			"did:plc:boltless", rkey, "sh.tangled.repo.issue.state", string(subject), []byte("{}"), created,
		); err != nil {
			t.Fatalf("insert %s: %v", rkey, err)
		}
	}

	insert("stale-orphan", "2000-01-01T00:00:00Z", orphan)
	insert("fresh-orphan", "2999-01-01T00:00:00Z", orphan)
	insert("stale-live", "2000-01-01T00:00:00Z", live)

	evicted, err := EvictStalePendingStateRecords(d, "2026-01-01T00:00:00Z")
	if err != nil {
		t.Fatalf("EvictStalePendingStateRecords: %v", err)
	}
	if evicted != 2 {
		t.Fatalf("both stale rows must be evicted regardless of subject presence, got %d want 2", evicted)
	}

	var remaining int
	if err := d.QueryRow(`select count(*) from pending_state_records`).Scan(&remaining); err != nil {
		t.Fatalf("count: %v", err)
	}
	if remaining != 1 {
		t.Fatalf("only the fresh row must survive the TTL sweep, got %d want 1", remaining)
	}
}
