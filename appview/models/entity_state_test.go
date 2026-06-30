package models

import (
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
)

const testIssueSubject = "at://did:plc:anemone/sh.tangled.repo.issue/i1"

func TestIssueStateFromRecord(t *testing.T) {
	const rkey = "3jzfcijpj2z2a"

	if _, err := IssueStateFromRecord("did:plc:akshay", rkey, tangled.RepoIssueState{
		Issue:     testIssueSubject,
		State:     "sh.tangled.repo.issue.state.reopened",
		CreatedAt: "2026-06-01T00:00:00Z",
	}); err == nil {
		t.Fatal("unknown state variant must be rejected")
	}

	created := "2026-06-01T12:30:00Z"
	rec, err := IssueStateFromRecord("did:plc:akshay", rkey, tangled.RepoIssueState{
		Issue:     testIssueSubject,
		State:     tangled.RepoIssueStateClosed,
		CreatedAt: created,
	})
	if err != nil {
		t.Fatalf("IssueStateFromRecord: %v", err)
	}
	want, _ := time.Parse(time.RFC3339, created)
	if rec.Value != StateClosed || rec.SortMicros != want.UnixMicro() {
		t.Fatalf("got value=%q micros=%d, want closed with createdAt as the sort key", rec.Value, rec.SortMicros)
	}

	tidRec, err := IssueStateFromRecord("did:plc:akshay", rkey, tangled.RepoIssueState{
		Issue:     testIssueSubject,
		State:     tangled.RepoIssueStateOpen,
		CreatedAt: "not-a-datetime",
	})
	if err != nil {
		t.Fatalf("IssueStateFromRecord tid fallback: %v", err)
	}
	tid, _ := syntax.ParseTID(rkey)
	if tidRec.SortMicros != tid.Time().UnixMicro() {
		t.Fatalf("SortMicros = %d, want TID micros %d when createdAt is unparseable", tidRec.SortMicros, tid.Time().UnixMicro())
	}
}

func TestPullStatusFromRecord_Variants(t *testing.T) {
	subject := "at://did:plc:limpet/sh.tangled.repo.pull/p1"
	cases := map[string]StateValue{
		tangled.RepoPullStatusOpen:   StateOpen,
		tangled.RepoPullStatusClosed: StateClosed,
		tangled.RepoPullStatusMerged: StateMerged,
	}
	for wire, want := range cases {
		rec, err := PullStatusFromRecord("did:plc:akshay", "3jzfcijpj2z2a", tangled.RepoPullStatus{
			Pull:      subject,
			Status:    wire,
			CreatedAt: "2026-06-01T00:00:00Z",
		})
		if err != nil {
			t.Fatalf("PullStatusFromRecord(%q): %v", wire, err)
		}
		if rec.Value != want {
			t.Fatalf("PullStatusFromRecord(%q) value = %q, want %q", wire, rec.Value, want)
		}
	}

	if _, err := PullStatusFromRecord("did:plc:akshay", "3jzfcijpj2z2a", tangled.RepoPullStatus{
		Pull:      subject,
		Status:    "sh.tangled.repo.pull.status.draft",
		CreatedAt: "2026-06-01T00:00:00Z",
	}); err == nil {
		t.Fatal("unknown pull status variant must be rejected")
	}
}

func TestAsIssueStateRecord_RoundTrip(t *testing.T) {
	const rkey = "3jzfcijpj2z2a"
	subject := syntax.ATURI(testIssueSubject)
	created := time.Date(2026, 6, 30, 12, 30, 15, int(123*time.Millisecond), time.UTC)

	for _, value := range []StateValue{StateOpen, StateClosed} {
		rec, err := AsIssueStateRecord(subject, value, created)
		if err != nil {
			t.Fatalf("AsIssueStateRecord(%q): %v", value, err)
		}
		if rec.Issue != testIssueSubject {
			t.Fatalf("Issue = %q, want %q", rec.Issue, testIssueSubject)
		}

		back, err := IssueStateFromRecord("did:plc:akshay", rkey, rec)
		if err != nil {
			t.Fatalf("IssueStateFromRecord after AsIssueStateRecord(%q): %v", value, err)
		}
		if back.Value != value {
			t.Fatalf("round-trip value = %q, want %q", back.Value, value)
		}
		if back.SortMicros != created.UnixMicro() {
			t.Fatalf("round-trip SortMicros = %d, want %d", back.SortMicros, created.UnixMicro())
		}
	}

	if _, err := AsIssueStateRecord(subject, StateMerged, created); err == nil {
		t.Fatal("merged is not a valid issue state and must be rejected")
	}
}

func TestAsPullStatusRecords_RoundTrip(t *testing.T) {
	const rkey = "3jzfcijpj2z2a"
	subject := syntax.ATURI("at://did:plc:limpet/sh.tangled.repo.pull/p1")
	created := time.Date(2026, 6, 30, 12, 30, 15, int(123*time.Millisecond), time.UTC)

	for _, value := range []StateValue{StateOpen, StateClosed, StateMerged} {
		recs, err := AsPullStatusRecords([]syntax.ATURI{subject}, value, created)
		if err != nil {
			t.Fatalf("AsPullStatusRecords(%q): %v", value, err)
		}
		if len(recs) != 1 {
			t.Fatalf("AsPullStatusRecords(%q) returned %d records, want 1", value, len(recs))
		}
		rec := recs[0]
		if rec.Pull != subject.String() {
			t.Fatalf("Pull = %q, want %q", rec.Pull, subject)
		}

		back, err := PullStatusFromRecord("did:plc:akshay", rkey, rec)
		if err != nil {
			t.Fatalf("PullStatusFromRecord after AsPullStatusRecords(%q): %v", value, err)
		}
		if back.Value != value {
			t.Fatalf("round-trip value = %q, want %q", back.Value, value)
		}
		if back.SortMicros != created.UnixMicro() {
			t.Fatalf("round-trip SortMicros = %d, want %d", back.SortMicros, created.UnixMicro())
		}
	}

	if _, err := AsPullStatusRecords([]syntax.ATURI{subject}, "bogus", created); err == nil {
		t.Fatal("invalid pull status value must be rejected")
	}
}
