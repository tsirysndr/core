package db

import (
	"context"
	"fmt"
	"path/filepath"
	"testing"

	"tangled.org/core/notifier"
)

func TestMillLeaseRoundTrip(t *testing.T) {
	d := newTestDB(t)

	lease := MillLease{
		LeaseID:  "lease-1",
		NodeID:   "node-1",
		Epoch:    "inc-1",
		Engine:   "dummy",
		Knot:     "knot.example",
		Rkey:     "rkey1",
		Workflow: "build",
		State:    "reserved",
	}
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}

	lease.State = "running"
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease(transition): %v", err)
	}

	leases, err := d.ListMillLeases()
	if err != nil {
		t.Fatalf("ListMillLeases: %v", err)
	}
	if len(leases) != 1 {
		t.Fatalf("ListMillLeases returned %d leases, want 1 (state transition must replace, not duplicate)", len(leases))
	}
	if leases[0].LeaseID != lease.LeaseID || leases[0].State != "running" {
		t.Fatalf("ListMillLeases[0] = %+v, want %+v", leases[0], lease)
	}

	if err := d.DeleteMillLease("lease-1"); err != nil {
		t.Fatalf("DeleteMillLease: %v", err)
	}
	if leases, _ = d.ListMillLeases(); len(leases) != 0 {
		t.Fatalf("lease survived deletion: %+v", leases)
	}
}

func TestExecutorCursors(t *testing.T) {
	d := newTestDB(t)

	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 5) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}
	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 9) }); err != nil {
		t.Fatalf("AdvanceCursor(advance): %v", err)
	}
	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-2", "inc-1", 1) }); err != nil {
		t.Fatalf("AdvanceCursor(node-2): %v", err)
	}

	cursors, err := d.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 2 {
		t.Fatalf("ListExecutorCursors = %v, want 2 cursors", cursors)
	}

	cursorMap := make(map[string]uint64)
	for _, c := range cursors {
		cursorMap[c.NodeID+"/"+c.Epoch] = c.AckedSeqno
	}

	if cursorMap["node-1/inc-1"] != 9 || cursorMap["node-2/inc-1"] != 1 {
		t.Fatalf("unexpected cursors: %v", cursorMap)
	}

}

func TestCompleteMillLeaseIsAtomic(t *testing.T) {
	d := newTestDB(t)
	lease := MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "knot.example", Rkey: "rkey1", Workflow: "build", State: "running",
	}
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}
	if _, err := d.Exec(`
		create trigger reject_mill_lease_delete
		before delete on mill_leases
		begin
			select raise(abort, 'forced delete failure');
		end
	`); err != nil {
		t.Fatalf("create failure trigger: %v", err)
	}

	n := notifier.New()
	notifications := n.Subscribe()
	defer n.Unsubscribe(notifications)
	err := d.CompleteMillLease(
		"lease-1",
		"at://knot.example/sh.tangled.pipeline/rkey1",
		"build",
		"failed",
		nil,
		nil,
		&n,
	)
	if err == nil {
		t.Fatal("CompleteMillLease succeeded despite forced lease deletion failure")
	}
	var eventCount int
	if err := d.QueryRow(`select count(*) from events`).Scan(&eventCount); err != nil {
		t.Fatalf("count events after rollback: %v", err)
	}
	if eventCount != 0 {
		t.Fatalf("terminal event count after rollback = %d, want 0", eventCount)
	}
	if leases, listErr := d.ListMillLeases(); listErr != nil || len(leases) != 1 {
		t.Fatalf("leases after rollback = %+v, err = %v; want original lease", leases, listErr)
	}
	select {
	case <-notifications:
		t.Fatal("rollback notified event subscribers")
	default:
	}

	if _, err := d.Exec(`drop trigger reject_mill_lease_delete`); err != nil {
		t.Fatalf("drop failure trigger: %v", err)
	}
	if err := d.CompleteMillLease(
		"lease-1",
		"at://knot.example/sh.tangled.pipeline/rkey1",
		"build",
		"failed",
		nil,
		nil,
		&n,
	); err != nil {
		t.Fatalf("CompleteMillLease retry: %v", err)
	}
	if err := d.QueryRow(`select count(*) from events`).Scan(&eventCount); err != nil {
		t.Fatalf("count committed events: %v", err)
	}
	if eventCount != 1 {
		t.Fatalf("terminal event count after commit = %d, want 1", eventCount)
	}
	if leases, listErr := d.ListMillLeases(); listErr != nil || len(leases) != 0 {
		t.Fatalf("leases after commit = %+v, err = %v; want none", leases, listErr)
	}
	select {
	case <-notifications:
	default:
		t.Fatal("committed terminal event did not notify subscribers")
	}
}

func TestRestartPersistence(t *testing.T) {
	dbPath := filepath.Join(t.TempDir(), "persist.db")
	ctx := context.Background()
	d, err := Make(ctx, dbPath)
	if err != nil {
		t.Fatalf("Make: %v", err)
	}

	lease := MillLease{
		LeaseID: "lease-p", NodeID: "node-p", Epoch: "inc-p", Engine: "dummy",
		Knot: "k", Rkey: "r", Workflow: "w", State: "running",
	}
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}

	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-p", "inc-p", 42) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}

	if err := d.SetOutboxEpoch("inc-p"); err != nil {
		t.Fatalf("SetOutboxEpoch: %v", err)
	}
	if _, err := d.AppendOutboxRow([]byte("hello world"), true); err != nil {
		t.Fatalf("AppendOutboxRow: %v", err)
	}

	if err := d.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	d2, err := Make(ctx, dbPath)
	if err != nil {
		t.Fatalf("Make reopen: %v", err)
	}
	defer d2.Close()

	leases, err := d2.ListMillLeases()
	if err != nil {
		t.Fatalf("ListMillLeases: %v", err)
	}
	if len(leases) != 1 || leases[0].LeaseID != "lease-p" || leases[0].Epoch != "inc-p" {
		t.Fatalf("unexpected leases: %+v", leases)
	}

	cursors, err := d2.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 1 || cursors[0].NodeID != "node-p" || cursors[0].Epoch != "inc-p" || cursors[0].AckedSeqno != 42 {
		t.Fatalf("unexpected cursors: %+v", cursors)
	}

	inc, nextSeqno, err := d2.GetOutboxState()
	if err != nil {
		t.Fatalf("GetOutboxState: %v", err)
	}
	if inc != "inc-p" || nextSeqno != 2 {
		t.Fatalf("unexpected outbox state: inc=%q, next=%d", inc, nextSeqno)
	}
	rows, err := d2.ListOutboxRows()
	if err != nil {
		t.Fatalf("ListOutboxRows: %v", err)
	}
	if len(rows) != 1 || string(rows[0].Payload) != "hello world" || !rows[0].Control {
		t.Fatalf("unexpected outbox rows: %+v", rows)
	}
}

func TestOutboxPrefixAck(t *testing.T) {
	d := newTestDB(t)

	if err := d.SetOutboxEpoch("inc-1"); err != nil {
		t.Fatalf("SetOutboxEpoch: %v", err)
	}

	o1, err := d.AppendOutboxRow([]byte("msg1"), false)
	if err != nil || o1 != 1 {
		t.Fatalf("AppendOutboxRow 1: %v, seqno=%d", err, o1)
	}
	o2, err := d.AppendOutboxRow([]byte("msg2"), false)
	if err != nil || o2 != 2 {
		t.Fatalf("AppendOutboxRow 2: %v, seqno=%d", err, o2)
	}
	o3, err := d.AppendOutboxRow([]byte("msg3"), true)
	if err != nil || o3 != 3 {
		t.Fatalf("AppendOutboxRow 3: %v, seqno=%d", err, o3)
	}

	n, err := d.DeleteOutboxPrefix(2)
	if err != nil {
		t.Fatalf("DeleteOutboxPrefix: %v", err)
	}
	if n.Rows != 2 || n.Bytes != 8 {
		t.Fatalf("deleted prefix = %+v, want 2 rows and 8 bytes", n)
	}

	rows, err := d.ListOutboxRows()
	if err != nil {
		t.Fatalf("ListOutboxRows: %v", err)
	}
	if len(rows) != 1 || rows[0].Seqno != 3 || string(rows[0].Payload) != "msg3" {
		t.Fatalf("expected only msg3 (seqno 3) to remain, got: %+v", rows)
	}
}

func TestCompositeCursors(t *testing.T) {
	d := newTestDB(t)

	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 10) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}
	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-2", 20) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}
	if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error { return tx.AdvanceCursor("node-2", "inc-1", 5) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}

	cursors, err := d.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 3 {
		t.Fatalf("expected 3 cursors, got %d", len(cursors))
	}

	cursorMap := make(map[string]uint64)
	for _, c := range cursors {
		key := c.NodeID + "/" + c.Epoch
		cursorMap[key] = c.AckedSeqno
	}

	if cursorMap["node-1/inc-1"] != 10 || cursorMap["node-1/inc-2"] != 20 || cursorMap["node-2/inc-1"] != 5 {
		t.Fatalf("unexpected cursor values: %v", cursorMap)
	}

}

func TestBatchRollback(t *testing.T) {
	d := newTestDB(t)

	lease := MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "k", Rkey: "r", Workflow: "w", State: "running",
	}
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}

	n := notifier.New()
	err := d.ApplyEventBatch(&n, func(tx *EventBatchTx) error {
		if err := tx.DeleteLease("lease-1"); err != nil {
			return err
		}
		if err := tx.AdvanceCursor("node-1", "inc-1", 100); err != nil {
			return err
		}
		return fmt.Errorf("forced batch failure")
	})

	if err == nil {
		t.Fatal("expected ApplyEventBatch to return error")
	}

	leases, err := d.ListMillLeases()
	if err != nil {
		t.Fatalf("ListMillLeases: %v", err)
	}
	if len(leases) != 1 {
		t.Fatalf("lease was deleted despite rollback: %+v", leases)
	}

	cursors, err := d.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 0 {
		t.Fatalf("cursor was advanced despite rollback: %+v", cursors)
	}
}

func TestTerminalCursorAtomicity(t *testing.T) {
	d := newTestDB(t)

	lease := MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "k", Rkey: "r", Workflow: "w", State: "running",
	}
	if err := d.SaveMillLease(lease); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}

	n := notifier.New()
	notifications := n.Subscribe()
	defer n.Unsubscribe(notifications)

	err := d.ApplyEventBatch(&n, func(tx *EventBatchTx) error {
		if err := tx.DeleteLease("lease-1"); err != nil {
			return err
		}
		return tx.AdvanceCursor("node-1", "inc-1", 100)
	})

	if err != nil {
		t.Fatalf("ApplyEventBatch: %v", err)
	}

	leases, err := d.ListMillLeases()
	if err != nil {
		t.Fatalf("ListMillLeases: %v", err)
	}
	if len(leases) != 0 {
		t.Fatalf("lease not deleted: %+v", leases)
	}

	cursors, err := d.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 1 || cursors[0].AckedSeqno != 100 {
		t.Fatalf("cursor not advanced correctly: %+v", cursors)
	}

	select {
	case <-notifications:
	default:
		t.Fatal("notifier was not fired after batch commit")
	}
}

func TestExecutorCursorResetHelpers(t *testing.T) {
	d := newTestDB(t)

	seed := func(node, epoch string, seqno uint64) {
		if err := d.ApplyEventBatch(nil, func(tx *EventBatchTx) error {
			return tx.AdvanceCursor(node, epoch, seqno)
		}); err != nil {
			t.Fatalf("seed cursor: %v", err)
		}
	}
	seed("node-1", "inc-1", 7)
	seed("node-1", "inc-2", 3)
	seed("node-2", "inc-1", 42)

	if cur, err := d.GetExecutorCursor("node-1", "inc-1"); err != nil || cur != 7 {
		t.Fatalf("GetExecutorCursor = %d, %v; want 7", cur, err)
	}
	// an unknown stream looks the same as a fresh one
	if cur, err := d.GetExecutorCursor("node-1", "missing"); err != nil || cur != 0 {
		t.Fatalf("GetExecutorCursor(missing) = %d, %v; want 0", cur, err)
	}

	// skip-forward touches every epoch of the node and nothing else
	if n, err := d.SetExecutorCursors("node-1", 28); err != nil || n != 2 {
		t.Fatalf("SetExecutorCursors = %d rows, %v; want 2", n, err)
	}
	if cur, _ := d.GetExecutorCursor("node-1", "inc-2"); cur != 28 {
		t.Fatalf("SetExecutorCursors left inc-2 at %d, want 28", cur)
	}
	if cur, _ := d.GetExecutorCursor("node-2", "inc-1"); cur != 42 {
		t.Fatalf("SetExecutorCursors clobbered node-2: %d, want 42", cur)
	}

	if n, err := d.DeleteExecutorCursors("node-1"); err != nil || n != 2 {
		t.Fatalf("DeleteExecutorCursors = %d rows, %v; want 2", n, err)
	}
	if cur, _ := d.GetExecutorCursor("node-1", "inc-1"); cur != 0 {
		t.Fatalf("DeleteExecutorCursors left %d, want 0", cur)
	}
}

func TestClearPendingArtifacts(t *testing.T) {
	d := newTestDB(t)
	if err := d.SavePendingArtifact("lease-1", "build", "success", "", 0, "ref", "sha256:x"); err != nil {
		t.Fatalf("SavePendingArtifact: %v", err)
	}
	if err := d.ClearPendingArtifacts(); err != nil {
		t.Fatalf("ClearPendingArtifacts: %v", err)
	}
	if rows, err := d.ListPendingArtifacts(); err != nil || len(rows) != 0 {
		t.Fatalf("ListPendingArtifacts after clear = %+v, %v; want empty", rows, err)
	}
}
