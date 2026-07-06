package models

import (
	"slices"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func TestApplyLabelOps_FutureDatedOrderIndependent(t *testing.T) {
	def := &LabelDefinition{
		Did: "did:plc:boltless", Rkey: "st", Name: "status",
		ValueType: ValueType{Type: ConcreteTypeString},
		Scope:     []string{"sh.tangled.repo.issue"},
		Multiple:  false,
	}
	key := def.AtUri().String()
	ctx := &LabelApplicationCtx{Defs: map[string]*LabelDefinition{key: def}}
	subject := syntax.ATURI("at://did:plc:boltless/sh.tangled.repo.issue/issue1")

	mk := func(rkey, val string, performed time.Time) LabelOp {
		return LabelOp{
			Did: "did:plc:boltless", Rkey: rkey, Subject: subject,
			Operation: LabelOperationAdd, OperandKey: key, OperandValue: val,
			PerformedAt: performed,
		}
	}

	earlier := time.Date(2100, 1, 1, 0, 0, 0, 0, time.UTC)
	later := time.Date(2200, 1, 1, 0, 0, 0, 0, time.UTC)

	fold := func(ops []LabelOp) []string {
		st := NewLabelState()
		ctx.ApplyLabelOps(st, ops)
		out := st.LabelNameValues()
		slices.Sort(out)
		return out
	}

	forward := fold([]LabelOp{mk("aaa", "open", earlier), mk("bbb", "closed", later)})
	reverse := fold([]LabelOp{mk("bbb", "closed", later), mk("aaa", "open", earlier)})

	if !slices.Equal(forward, reverse) {
		t.Fatalf("fold must be order-independent for future-dated ops: forward=%v reverse=%v", forward, reverse)
	}
	if !slices.Equal(forward, []string{"status:closed"}) {
		t.Fatalf("the later createdAt must win regardless of ingest order, got %v", forward)
	}
}
