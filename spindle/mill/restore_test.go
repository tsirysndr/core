package mill

import (
	"context"
	"path/filepath"
	"testing"
	"time"

	"tangled.org/core/notifier"
	"tangled.org/core/spindle/db"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
)

func restoreTestMill(t *testing.T, cfg Config) (*Mill, *db.DB) {
	t.Helper()
	bdb, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()
	m := New(discardLogger(), cfg)
	m.Attach(bdb, &n)
	return m, bdb
}

func restoredMill(t *testing.T, bdb *db.DB, cfg Config) *Mill {
	t.Helper()
	n := notifier.New()
	m := New(discardLogger(), cfg)
	m.Attach(bdb, &n)
	if err := m.RestoreState(); err != nil {
		t.Fatalf("RestoreState: %v", err)
	}
	return m
}

func TestRestoreStateRebuildsLeasesAndCursors(t *testing.T) {
	_, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})

	if err := bdb.SaveMillLease(db.MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "knot.example", Rkey: "rkey1", Workflow: "build", State: leaseRowRunning,
	}); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}
	if err := bdb.ApplyEventBatch(nil, func(tx *db.EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 7) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}

	m2 := restoredMill(t, bdb, Config{ReconnectGrace: time.Minute})

	m2.mu.Lock()
	lease := m2.leases["lease-1"]
	seqno := m2.nodeSeqno["node-1/inc-1"]
	m2.mu.Unlock()

	if lease == nil {
		t.Fatal("restored mill has no lease-1")
	}
	if !lease.orphaned {
		t.Fatal("restored lease is not orphaned; a terminal would be delivered to a waiter that does not exist")
	}
	if lease.getState() != leaseRunning {
		t.Fatalf("restored lease state = %v, want leaseRunning", lease.getState())
	}
	wantWid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "knot.example", Rkey: "rkey1"}, Name: "build"}
	if lease.wid != wantWid {
		t.Fatalf("restored lease wid = %+v, want %+v", lease.wid, wantWid)
	}
	if seqno != 7 {
		t.Fatalf("restored cursor = %d, want 7", seqno)
	}
}

func TestOrphanTerminalAuthorsStatusRow(t *testing.T) {
	_, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	if err := bdb.SaveMillLease(db.MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "knot.example", Rkey: "rkey1", Workflow: "build", State: leaseRowRunning,
	}); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}
	if err := bdb.ApplyEventBatch(nil, func(tx *db.EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 3) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}

	m := restoredMill(t, bdb, Config{ReconnectGrace: time.Minute})

	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	resume, ok := m.attachSession(sess)
	if !ok {
		t.Fatal("attachSession rejected the reconnecting executor")
	}
	if resume != 3 {
		t.Fatalf("attachSession resume seqno = %d, want restored cursor 3", resume)
	}

	_ = m.onEventBatch(sess, &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   4,
				LeaseId: "lease-1",
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_SUCCESS,
					},
				},
			},
		},
	})

	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "knot.example", Rkey: "rkey1"}, Name: "build"}
	st, err := bdb.GetStatus(wid)
	if err != nil {
		t.Fatalf("GetStatus after orphan terminal: %v", err)
	}
	if st.Status != string(models.StatusKindSuccess) {
		t.Fatalf("orphan terminal authored status %q, want success", st.Status)
	}

	m.mu.Lock()
	_, still := m.leases["lease-1"]
	m.mu.Unlock()
	if still {
		t.Fatal("finished orphan still in the lease map")
	}
	if rows, _ := bdb.ListMillLeases(); len(rows) != 0 {
		t.Fatalf("finished orphan still persisted: %+v", rows)
	}
}

func TestSnapshotReconciliationFailsDroppedOrphans(t *testing.T) {
	_, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	for _, l := range []db.MillLease{
		{LeaseID: "lease-kept", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy", Knot: "k", Rkey: "r1", Workflow: "w", State: leaseRowRunning},
		{LeaseID: "lease-gone", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy", Knot: "k", Rkey: "r2", Workflow: "w", State: leaseRowRunning},
	} {
		if err := bdb.SaveMillLease(l); err != nil {
			t.Fatalf("SaveMillLease(%s): %v", l.LeaseID, err)
		}
	}

	m := restoredMill(t, bdb, Config{ReconnectGrace: time.Minute})

	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)

	m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno:          1,
		ActiveLeaseIds: []string{"lease-kept"},
	})

	m.mu.Lock()
	_, kept := m.leases["lease-kept"]
	_, gone := m.leases["lease-gone"]
	m.mu.Unlock()
	if !kept {
		t.Fatal("reconciliation dropped a lease the executor still holds")
	}
	if gone {
		t.Fatal("reconciliation kept a lease the executor no longer holds")
	}

	st, err := bdb.GetStatus(models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r2"}, Name: "w"})
	if err != nil {
		t.Fatalf("GetStatus for dropped orphan: %v", err)
	}
	if st.Status != string(models.StatusKindFailed) {
		t.Fatalf("dropped orphan authored status %q, want failed", st.Status)
	}
}
func TestSnapshotReconciliationPreservesRequestedCancellation(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	lease := newLease("lease-1", "node-1", "inc-old", "dummy")
	lease.wid = models.WorkflowId{
		PipelineId: models.PipelineId{Knot: "k", Rkey: "r1"},
		Name:       "w",
	}
	lease.setState(leaseRunning)
	lease.requestCancel("workflow destroyed")
	if err := m.persistLease(lease, leaseRowRunning); err != nil {
		t.Fatalf("persistLease: %v", err)
	}
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	sess := newSession("node-1", "inc-new", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)
	if err := m.onSnapshot(sess, &millv1.NodeSnapshot{Seqno: 1}); err != nil {
		t.Fatalf("onSnapshot: %v", err)
	}

	st, err := bdb.GetStatus(lease.wid)
	if err != nil {
		t.Fatalf("GetStatus: %v", err)
	}
	if st.Status != string(models.StatusKindCancelled) {
		t.Fatalf("reconciled status = %q, want cancelled", st.Status)
	}
}

func TestSnapshotReconciliationCancelsUnknownExecutorLease(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	cancelled := make(chan string, 1)
	sess := newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		if cancel := msg.GetCancelAttempt(); cancel != nil {
			cancelled <- cancel.GetLeaseId()
		}
		return nil
	}), discardLogger())
	m.attachSession(sess)

	if err := m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno:          1,
		ActiveLeaseIds: []string{"executor-only"},
	}); err != nil {
		t.Fatalf("onSnapshot: %v", err)
	}
	select {
	case id := <-cancelled:
		if id != "executor-only" {
			t.Fatalf("cancelled lease = %q, want executor-only", id)
		}
	default:
		t.Fatal("snapshot reconciliation left an executor-only lease running")
	}
}

func TestSweepFailsOrphansOfAbsentExecutors(t *testing.T) {
	_, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	if err := bdb.SaveMillLease(db.MillLease{
		LeaseID: "lease-1", NodeID: "node-absent", Epoch: "inc-absent", Engine: "dummy",
		Knot: "k", Rkey: "r1", Workflow: "w", State: leaseRowReserved,
	}); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}

	m := restoredMill(t, bdb, Config{ReconnectGrace: time.Minute})
	m.sweepUnclaimedOrphans()

	m.mu.Lock()
	_, still := m.leases["lease-1"]
	m.mu.Unlock()
	if still {
		t.Fatal("sweep kept an orphan whose executor never reconnected")
	}
	st, err := bdb.GetStatus(models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r1"}, Name: "w"})
	if err != nil {
		t.Fatalf("GetStatus after sweep: %v", err)
	}
	if st.Status != string(models.StatusKindFailed) {
		t.Fatalf("sweep authored status %q, want failed", st.Status)
	}
	if rows, _ := bdb.ListMillLeases(); len(rows) != 0 {
		t.Fatalf("swept orphan still persisted: %+v", rows)
	}
}

func TestAckSeqnoPersistsCursor(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)

	owned := newLease("lease-1", "node-1", "inc-1", "dummy")
	owned.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	m.mu.Lock()
	m.leases[owned.id] = owned
	m.mu.Unlock()

	err := m.onEventBatch(sess, &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: owned.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{
						Status: millv1.NonterminalStatus_RUNNING,
					},
				},
			},
		},
	})
	if err != nil {
		t.Fatalf("onEventBatch: %v", err)
	}

	cursors, err := bdb.ListExecutorCursors()
	if err != nil {
		t.Fatalf("ListExecutorCursors: %v", err)
	}
	if len(cursors) != 1 || cursors[0].AckedSeqno != 1 {
		t.Fatalf("persisted cursor = %+v, want seqno 1", cursors)
	}
}

func TestOrphanTerminalFailureKeepsLeaseAndSeqnoRetryable(t *testing.T) {
	_, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	if err := bdb.SaveMillLease(db.MillLease{
		LeaseID: "lease-1", NodeID: "node-1", Epoch: "inc-1", Engine: "dummy",
		Knot: "knot.example", Rkey: "rkey1", Workflow: "build", State: leaseRowRunning,
	}); err != nil {
		t.Fatalf("SaveMillLease: %v", err)
	}
	if err := bdb.ApplyEventBatch(nil, func(tx *db.EventBatchTx) error { return tx.AdvanceCursor("node-1", "inc-1", 3) }); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}
	if _, err := bdb.Exec(`
		create trigger reject_orphan_lease_delete
		before delete on mill_leases
		begin
			select raise(abort, 'forced delete failure');
		end
	`); err != nil {
		t.Fatalf("create failure trigger: %v", err)
	}

	m := restoredMill(t, bdb, Config{ReconnectGrace: time.Minute})
	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	if _, ok := m.attachSession(sess); !ok {
		t.Fatal("attachSession rejected reconnect")
	}
	batch := &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   4,
				LeaseId: "lease-1",
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_SUCCESS,
					},
				},
			},
		},
	}
	if err := m.onEventBatch(sess, batch); err == nil {
		t.Fatal("orphan terminal stream succeeded despite forced transaction failure")
	}

	m.mu.Lock()
	lease := m.leases["lease-1"]
	seqno := m.nodeSeqno["node-1/inc-1"]
	m.mu.Unlock()
	if lease == nil || lease.getState() == leaseDone {
		t.Fatal("failed orphan completion made the in-memory lease unretryable")
	}
	if seqno != 3 {
		t.Fatalf("in-memory stream seqno = %d, want 3", seqno)
	}
	if rows, err := bdb.ListMillLeases(); err != nil || len(rows) != 1 {
		t.Fatalf("durable leases after transaction rollback = %+v, err = %v; want retained lease", rows, err)
	}
	var events int
	if err := bdb.QueryRow(`select count(*) from events`).Scan(&events); err != nil {
		t.Fatalf("count events: %v", err)
	}
	if events != 0 {
		t.Fatalf("terminal events after transaction rollback = %d, want 0", events)
	}

	if _, err := bdb.Exec(`drop trigger reject_orphan_lease_delete`); err != nil {
		t.Fatalf("drop failure trigger: %v", err)
	}
	if err := m.onEventBatch(sess, batch); err != nil {
		t.Fatalf("retry orphan terminal: %v", err)
	}
	m.mu.Lock()
	_, still := m.leases["lease-1"]
	seqno = m.nodeSeqno["node-1/inc-1"]
	m.mu.Unlock()
	if still {
		t.Fatal("successful orphan completion retained in-memory lease")
	}
	if seqno != 4 {
		t.Fatalf("in-memory stream seqno after retry = %d, want 4", seqno)
	}
	if rows, err := bdb.ListMillLeases(); err != nil || len(rows) != 0 {
		t.Fatalf("durable leases after successful retry = %+v, err = %v; want none", rows, err)
	}
}
