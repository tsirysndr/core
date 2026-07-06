package appview

import (
	"context"
	"encoding/json"
	"slices"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

const (
	labelOwner   = "did:plc:boltless"
	labelRepoDid = "did:plc:anemone"
	labelPerform = "2026-06-01T00:00:00Z"
)

func newLabelIngester(t *testing.T) *Ingester {
	t.Helper()
	ing := newStateIngester(t)
	ing.Acl = stubAcl{allow: true}
	return ing
}

func seedLabelDef(t *testing.T, d *db.DB, repoDid, defDid, defRkey, name string, scope []string, multiple, subscribe bool) syntax.ATURI {
	t.Helper()
	def := &models.LabelDefinition{
		Did: defDid, Rkey: defRkey, Name: name,
		ValueType: models.ValueType{Type: models.ConcreteTypeString},
		Scope:     scope, Multiple: multiple, Created: time.Now(),
	}
	if _, err := db.AddLabelDefinition(d, def); err != nil {
		t.Fatalf("AddLabelDefinition: %v", err)
	}
	if subscribe {
		if err := db.SubscribeLabel(d, &models.RepoLabel{RepoDid: syntax.DID(repoDid), LabelAt: def.AtUri()}); err != nil {
			t.Fatalf("SubscribeLabel: %v", err)
		}
	}
	return def.AtUri()
}

func labelOpEvent(t *testing.T, op, did, rkey, subject, performedAt string, add, del [][2]string) *jmodels.Event {
	t.Helper()
	mk := func(pairs [][2]string) []*tangled.LabelOp_Operand {
		out := make([]*tangled.LabelOp_Operand, 0, len(pairs))
		for _, p := range pairs {
			out = append(out, &tangled.LabelOp_Operand{Key: p[0], Value: p[1]})
		}
		return out
	}
	raw, err := json.Marshal(tangled.LabelOp{Subject: subject, PerformedAt: performedAt, Add: mk(add), Delete: mk(del)})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return &jmodels.Event{Did: did, Kind: jmodels.EventKindCommit, Commit: &jmodels.Commit{
		Operation: op, Collection: tangled.LabelOpNSID, RKey: rkey, Record: raw}}
}

func labelDefEvent(t *testing.T, did, rkey, name string, scope []string, multiple bool) *jmodels.Event {
	t.Helper()
	vt := tangled.LabelDefinition_ValueType{Type: string(models.ConcreteTypeString), Format: string(models.ValueTypeFormatAny)}
	raw, err := json.Marshal(tangled.LabelDefinition{Name: name, Scope: scope, Multiple: &multiple, CreatedAt: labelPerform, ValueType: &vt})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return &jmodels.Event{Did: did, Kind: jmodels.EventKindCommit, Commit: &jmodels.Commit{
		Operation: jmodels.CommitOperationCreate, Collection: tangled.LabelDefinitionNSID, RKey: rkey, Record: raw}}
}

func mustIngestOp(t *testing.T, ing *Ingester, e *jmodels.Event) {
	t.Helper()
	if err := ing.ingestLabelOp(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestLabelOp: %v", err)
	}
}

func issueAt(owner, rkey string) syntax.ATURI {
	return syntax.ATURI("at://" + owner + "/" + tangled.RepoIssueNSID + "/" + rkey)
}

func subjectLabels(t *testing.T, d *db.DB, subject syntax.ATURI) []string {
	t.Helper()
	states, err := db.GetLabels(d, orm.FilterEq("subject", subject))
	if err != nil {
		t.Fatalf("GetLabels: %v", err)
	}
	vals := states[subject].LabelNameValues()
	slices.Sort(vals)
	return vals
}

func labelOpValues(t *testing.T, d *db.DB, did, rkey string) []string {
	t.Helper()
	ops, err := db.GetLabelOps(d, orm.FilterEq("did", did), orm.FilterEq("rkey", rkey))
	if err != nil {
		t.Fatalf("GetLabelOps: %v", err)
	}
	out := make([]string, 0, len(ops))
	for _, op := range ops {
		out = append(out, op.OperandValue)
	}
	slices.Sort(out)
	return out
}

func pendingCount(t *testing.T, d *db.DB, subject syntax.ATURI) int {
	t.Helper()
	p, err := db.PendingStateRecordsForSubject(d, subject)
	if err != nil {
		t.Fatalf("PendingStateRecordsForSubject: %v", err)
	}
	return len(p)
}

type labelStep struct {
	op, rkey, subj, perform string
	add, del                []string
}

type foldCase struct {
	name       string
	defRkey    string
	defName    string
	scope      []string
	multiple   bool
	unsub      bool
	steps      []labelStep
	want       map[string][]string
	reversible bool
}

func runFold(t *testing.T, tc foldCase, steps []labelStep) map[string][]string {
	t.Helper()
	ing := newLabelIngester(t)
	defUri := "at://" + labelOwner + "/" + tangled.LabelDefinitionNSID + "/" + tc.defRkey

	seedRepo(t, ing.Db, labelOwner, labelRepoDid)
	seeded := map[string]bool{}
	for _, s := range steps {
		if !seeded[s.subj] {
			seedIssue(t, ing.Db, labelOwner, labelRepoDid, s.subj)
			seeded[s.subj] = true
		}
	}
	seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, tc.defRkey, tc.defName, tc.scope, tc.multiple, !tc.unsub)

	pairs := func(vals []string) [][2]string {
		out := make([][2]string, 0, len(vals))
		for _, v := range vals {
			out = append(out, [2]string{defUri, v})
		}
		return out
	}
	kind := map[string]string{"create": jmodels.CommitOperationCreate, "update": jmodels.CommitOperationUpdate, "delete": jmodels.CommitOperationDelete}
	for _, s := range steps {
		perform := s.perform
		if perform == "" {
			perform = labelPerform
		}
		mustIngestOp(t, ing, labelOpEvent(t, kind[s.op], labelOwner, s.rkey, string(issueAt(labelOwner, s.subj)), perform, pairs(s.add), pairs(s.del)))
	}

	got := map[string][]string{}
	for subj := range tc.want {
		at := issueAt(labelOwner, subj)
		got[subj] = subjectLabels(t, ing.Db, at)
		if n := pendingCount(t, ing.Db, at); n != 0 {
			t.Fatalf("fold case %q must not park %s, pending=%d", tc.name, subj, n)
		}
	}
	return got
}

func TestIngestLabelOp_Fold(t *testing.T) {
	issue := []string{tangled.RepoIssueNSID}
	pull := []string{tangled.RepoPullNSID}

	tests := []foldCase{{
		name: "delete removes the label", defRkey: "prio", defName: "priority", scope: issue, multiple: true,
		steps: []labelStep{{op: "create", rkey: "op1", subj: "issue1", add: []string{"high"}}, {op: "delete", rkey: "op1", subj: "issue1"}},
		want:  map[string][]string{"issue1": nil},
	}, {
		name: "update drops operands the new record no longer carries", defRkey: "prio", defName: "priority", scope: issue, multiple: true,
		steps: []labelStep{{op: "create", rkey: "op1", subj: "issue1", add: []string{"high", "low"}}, {op: "update", rkey: "op1", subj: "issue1", add: []string{"high"}}},
		want:  map[string][]string{"issue1": {"priority:high"}},
	}, {
		name: "update moving subject leaves no stale label", defRkey: "prio", defName: "priority", scope: issue, multiple: true,
		steps: []labelStep{{op: "create", rkey: "op1", subj: "issue1", add: []string{"high"}}, {op: "update", rkey: "op1", subj: "issue2", add: []string{"high"}}},
		want:  map[string][]string{"issue1": nil, "issue2": {"priority:high"}},
	}, {
		name: "a pull-scoped label on an issue is dropped by the fold", defRkey: "prio", defName: "priority", scope: pull, multiple: true,
		steps: []labelStep{{op: "create", rkey: "op1", subj: "issue1", add: []string{"high"}}},
		want:  map[string][]string{"issue1": nil}, reversible: true,
	}, {
		name: "a globally-defined but unsubscribed def still applies", defRkey: "prio", defName: "priority", scope: issue, multiple: true, unsub: true,
		steps: []labelStep{{op: "create", rkey: "op1", subj: "issue1", add: []string{"high"}}},
		want:  map[string][]string{"issue1": {"priority:high"}}, reversible: true,
	}, {
		name: "a same-instant tie resolves by source rkey", defRkey: "st", defName: "status", scope: issue, multiple: false,
		steps: []labelStep{{op: "create", rkey: "aaa", subj: "issue1", add: []string{"open"}}, {op: "create", rkey: "bbb", subj: "issue1", add: []string{"closed"}}},
		want:  map[string][]string{"issue1": {"status:closed"}}, reversible: true,
	}}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			orders := [][]labelStep{tc.steps}
			if tc.reversible {
				rev := slices.Clone(tc.steps)
				slices.Reverse(rev)
				orders = append(orders, rev)
			}
			for _, steps := range orders {
				got := runFold(t, tc, steps)
				for subj, want := range tc.want {
					if !slices.Equal(got[subj], want) {
						t.Fatalf("subject %s: got %v want %v", subj, got[subj], want)
					}
				}
			}
		})
	}
}

func TestIngestLabelOp_ReingestIsIdempotent(t *testing.T) {
	ing := newLabelIngester(t)
	subject := seedRepoAndIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
	def := seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, "prio", "priority", []string{tangled.RepoIssueNSID}, true, true)
	e := labelOpEvent(t, jmodels.CommitOperationCreate, labelOwner, "op1", string(subject), labelPerform,
		[][2]string{{def.String(), "high"}, {def.String(), "low"}}, nil)

	mustIngestOp(t, ing, e)
	first := labelOpValues(t, ing.Db, labelOwner, "op1")
	mustIngestOp(t, ing, e)
	if second := labelOpValues(t, ing.Db, labelOwner, "op1"); !slices.Equal(first, second) {
		t.Fatalf("re-ingesting an unchanged record changed its stored ops: %v -> %v", first, second)
	}
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high", "priority:low"}) {
		t.Fatalf("re-ingest changed the derived set: %v", got)
	}
}

func TestIngestLabelOp_Parking(t *testing.T) {
	defUri := "at://" + labelOwner + "/" + tangled.LabelDefinitionNSID + "/prio"
	issue := []string{tangled.RepoIssueNSID}
	seedDef := func(ing *Ingester) syntax.ATURI {
		return seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, "prio", "priority", issue, true, true)
	}

	tests := []struct {
		name       string
		missing    string
		acl        bool
		author     string
		trigger    string
		wantLabels []string
	}{
		{"missing subject parks, drains on reconcile", "subject", true, labelOwner, "seed-subject", []string{"priority:high"}},
		{"missing def parks, drains on reconcile", "def", true, labelOwner, "seed-def", []string{"priority:high"}},
		{"def create drains a parked op without the sweep", "def", true, labelOwner, "def-event", []string{"priority:high"}},
		{"deleting a parked op unparks it and a later def does not resurrect it", "def", true, labelOwner, "delete-then-def", nil},
		{"a parked op that fails auth on drain is dropped", "subject", false, "did:plc:squid", "seed-subject", nil},
		{"a missing-def park is evicted by TTL though its subject exists", "def", true, labelOwner, "evict", nil},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			ing := newLabelIngester(t)
			if !tt.acl {
				ing.Acl = stubAcl{}
			}
			subject := issueAt(labelOwner, "issue1")
			seedRepo(t, ing.Db, labelOwner, labelRepoDid)
			if tt.missing != "subject" {
				seedIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
			}
			if tt.missing != "def" {
				seedDef(ing)
			}

			mustIngestOp(t, ing, labelOpEvent(t, jmodels.CommitOperationCreate, tt.author, "op1", string(subject), labelPerform,
				[][2]string{{defUri, "high"}}, nil))
			if n := pendingCount(t, ing.Db, subject); n != 1 {
				t.Fatalf("op must park while its %s is missing, pending=%d", tt.missing, n)
			}

			switch tt.trigger {
			case "seed-subject":
				seedIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
				ing.ReconcilePendingState()
			case "seed-def":
				seedDef(ing)
				ing.ReconcilePendingState()
			case "def-event":
				if err := ing.ingestLabelDefinition(labelDefEvent(t, labelOwner, "prio", "priority", issue, true), ing.Logger); err != nil {
					t.Fatalf("ingest def: %v", err)
				}
			case "delete-then-def":
				mustIngestOp(t, ing, labelOpEvent(t, jmodels.CommitOperationDelete, tt.author, "op1", string(subject), "", nil, nil))
				if n := pendingCount(t, ing.Db, subject); n != 0 {
					t.Fatalf("deleting a parked op must unpark it, pending=%d", n)
				}
				seedDef(ing)
				ing.ReconcilePendingState()
			case "evict":
				if n, err := db.EvictStalePendingStateRecords(ing.Db, "2999-01-01T00:00:00Z"); err != nil {
					t.Fatalf("evict: %v", err)
				} else if n != 1 {
					t.Fatalf("a stale missing-def park must be evicted, got %d", n)
				}
			}

			if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, tt.wantLabels) {
				t.Fatalf("after %q: got %v want %v", tt.trigger, got, tt.wantLabels)
			}
			if n := pendingCount(t, ing.Db, subject); n != 0 {
				t.Fatalf("no record must remain parked, pending=%d", n)
			}
		})
	}
}

func TestIngestLabelOp_DeauthorizedUpdatePreservesPriorOps(t *testing.T) {
	ing := newLabelIngester(t)
	author := "did:plc:squid"
	subject := seedRepoAndIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
	def := seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, "prio", "priority", []string{tangled.RepoIssueNSID}, true, true)

	mustIngestOp(t, ing, labelOpEvent(t, jmodels.CommitOperationCreate, author, "op1", string(subject), labelPerform,
		[][2]string{{def.String(), "high"}}, nil))
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high"}) {
		t.Fatalf("authorized create: got %v", got)
	}

	ing.Acl = stubAcl{}
	mustIngestOp(t, ing, labelOpEvent(t, jmodels.CommitOperationUpdate, author, "op1", string(subject), labelPerform,
		[][2]string{{def.String(), "low"}}, nil))
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high"}) {
		t.Fatalf("a now-unauthorized update must neither apply nor erase the prior authorized op, got %v", got)
	}
}

func TestIngestLabelOp_DeauthorizedRedeliveryPreservesPriorOps(t *testing.T) {
	ing := newLabelIngester(t)
	author := "did:plc:squid"
	subject := seedRepoAndIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
	def := seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, "prio", "priority", []string{tangled.RepoIssueNSID}, true, true)

	create := labelOpEvent(t, jmodels.CommitOperationCreate, author, "op1", string(subject), labelPerform,
		[][2]string{{def.String(), "high"}}, nil)
	mustIngestOp(t, ing, create)
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high"}) {
		t.Fatalf("authorized create: got %v", got)
	}

	ing.Acl = stubAcl{}
	mustIngestOp(t, ing, create)
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high"}) {
		t.Fatalf("a redelivered create must not erase a label applied while authorized, got %v", got)
	}
}

func TestIngestLabelOp_DefDeleteDropsOps(t *testing.T) {
	ing := newLabelIngester(t)
	subject := seedRepoAndIssue(t, ing.Db, labelOwner, labelRepoDid, "issue1")
	def := seedLabelDef(t, ing.Db, labelRepoDid, labelOwner, "prio", "priority", []string{tangled.RepoIssueNSID}, true, true)

	mustIngestOp(t, ing, labelOpEvent(t, jmodels.CommitOperationCreate, labelOwner, "op1", string(subject), labelPerform,
		[][2]string{{def.String(), "high"}}, nil))
	if got := subjectLabels(t, ing.Db, subject); !slices.Equal(got, []string{"priority:high"}) {
		t.Fatalf("after create: got %v", got)
	}

	del := &jmodels.Event{Did: labelOwner, Kind: jmodels.EventKindCommit, Commit: &jmodels.Commit{
		Operation: jmodels.CommitOperationDelete, Collection: tangled.LabelDefinitionNSID, RKey: "prio"}}
	if err := ing.ingestLabelDefinition(del, ing.Logger); err != nil {
		t.Fatalf("ingest def delete: %v", err)
	}
	if rows := labelOpValues(t, ing.Db, labelOwner, "op1"); len(rows) != 0 {
		t.Fatalf("deleting a def must cascade-delete its label_ops rows, survived: %v", rows)
	}
	if got := subjectLabels(t, ing.Db, subject); len(got) != 0 {
		t.Fatalf("deleting a def must drop its labels, got %v", got)
	}
}
