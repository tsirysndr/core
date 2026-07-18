package mill

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"sync"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"

	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

type scriptedEncoder func(*millproto.Message) error

func (e scriptedEncoder) Encode(msg *millproto.Message) error { return e(msg) }

func testWorkflow(name string) *models.Workflow {
	return &models.Workflow{
		Name:        name,
		Environment: map[string]string{},
		Steps:       []models.Step{remoteStep{}},
		Data: &millWorkflowState{
			RawWorkflow: tangled.Pipeline_Workflow{Name: name},
			RawPipeline: tangled.Pipeline{TriggerMetadata: &tangled.Pipeline_TriggerMetadata{}},
		},
	}
}

func testWorkflowWithRunsOn(name string, runsOn []string) *models.Workflow {
	wf := testWorkflow(name)
	wf.Data.(*millWorkflowState).RawWorkflow.RunsOn = runsOn
	return wf
}

func addCandidateSession(t *testing.T, m *Mill, nodeID string, labels []string, load float64, enc messageEncoder) *millSession {
	t.Helper()
	if enc == nil {
		enc = scriptedEncoder(func(*millproto.Message) error { return nil })
	}
	sess := newSession(nodeID, "inc-"+nodeID, labels, enc, slog.New(slog.NewTextHandler(io.Discard, nil)))
	sess.snapshot = &millv1.NodeSnapshot{
		Seqno: 1,
		Engines: map[string]*millv1.EngineAvailability{
			"dummy": {Available: load < 1.0, Load: map[string]float64{"slots": load}},
		},
	}
	m.mu.Lock()
	m.sessions[nodeID] = sess
	m.mu.Unlock()
	return sess
}

func assertRankedNodes(t *testing.T, got []*millSession, want []string) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("rankCandidates() returned %d candidates, want %d: got %v want %v", len(got), len(want), sessionIDs(got), want)
	}
	for i := range want {
		if got[i].nodeID != want[i] {
			t.Fatalf("rankCandidates()[%d] = %q, want %q; full order got %v want %v", i, got[i].nodeID, want[i], sessionIDs(got), want)
		}
	}
}

func sessionIDs(sessions []*millSession) []string {
	out := make([]string, len(sessions))
	for i, sess := range sessions {
		out[i] = sess.nodeID
	}
	return out
}

func sameStringMultiset(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	counts := make(map[string]int, len(a))
	for _, s := range a {
		counts[s]++
	}
	for _, s := range b {
		if counts[s] == 0 {
			return false
		}
		counts[s]--
	}
	return true
}

type reserveReply struct {
	accepted    bool
	rejectClass millv1.RejectClass
	reason      string
}

func addReplyingCandidateSession(t *testing.T, m *Mill, nodeID string, labels []string, load float64, asked chan<- string, reply reserveReply) *millSession {
	t.Helper()
	var sess *millSession
	sess = addCandidateSession(t, m, nodeID, labels, load, scriptedEncoder(func(msg *millproto.Message) error {
		rs := msg.GetReserveSeat()
		if rs == nil {
			return nil
		}
		if asked != nil {
			asked <- nodeID
		}
		sess.deliver(rs.GetLeaseId(), &millproto.Message{ReserveResult: &millv1.ReserveResult{
			LeaseId:      rs.GetLeaseId(),
			Accepted:     reply.accepted,
			RejectReason: reply.reason,
			RejectClass:  reply.rejectClass,
		}})
		return nil
	}))
	return sess
}

func drainAsked(ch <-chan string) []string {
	var out []string
	for {
		select {
		case nodeID := <-ch:
			out = append(out, nodeID)
		default:
			return out
		}
	}
}

func TestCommitRetriesAfterSessionCloseBeforeCommitted(t *testing.T) {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	m := New(l, Config{BidTimeout: 25 * time.Millisecond, ReconnectGrace: time.Second})
	wf := testWorkflow("build")
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = wid
	wf.Data.(*millWorkflowState).Lease = lease

	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	var sess1 *millSession
	firstCommit := make(chan struct{})
	sess1 = newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		if msg.GetCommitLease() != nil {
			close(firstCommit)
			m.detachSession(sess1)
		}
		return nil
	}), l)
	m.attachSession(sess1)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- m.commitAndWait(ctx, wf, nil) }()

	select {
	case <-firstCommit:
	case <-ctx.Done():
		t.Fatal("first commit was not sent")
	}

	var sess2 *millSession
	sess2 = newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		if msg.GetCommitLease() == nil {
			return nil
		}
		leaseID := msg.GetCommitLease().GetLeaseId()
		sess2.deliver(leaseID, &millproto.Message{Committed: &millv1.Committed{LeaseId: leaseID}})
		_ = m.onEventBatch(sess2, &millv1.EventBatch{
			Epoch: sess2.epoch,
			Events: []*millv1.Event{
				{
					Seqno:   1,
					LeaseId: leaseID,
					Payload: &millv1.Event_AttemptResult{
						AttemptResult: &millv1.AttemptResult{
							Status: millv1.TerminalStatus_SUCCESS,
						},
					},
				},
			},
		})
		return nil
	}), l)
	m.attachSession(sess2)
	m.sessionReady(sess2)

	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("commitAndWait() error = %v, want success after reconnect", err)
		}
	case <-ctx.Done():
		t.Fatal("commitAndWait() did not finish after reconnect")
	}
}

func TestDestroyRunningLeaseDoesNotDropCancelledTerminal(t *testing.T) {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	m := New(l, Config{})
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = wid
	lease.setState(leaseRunning)

	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	m.destroy(wid)
	if lease.getState() == leaseDone {
		t.Fatal("destroy sealed the lease before the terminal result")
	}

	lease.deliverTerminal(&millv1.AttemptResult{
		Status: millv1.TerminalStatus_CANCELLED,
	})
	res := <-lease.terminal
	if err := terminalError(res.Status); !errors.Is(err, engine.ErrWorkflowCanceled) {
		t.Fatalf("terminalError() = %v, want ErrCancelled", err)
	}
}

func TestPlaceBlocksWhenNoCapacity(t *testing.T) {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	m := New(l, Config{})
	wf := testWorkflow("build")
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}

	// with no executors at all, place must block until ctx expires and the user sees pending
	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()

	_, err := m.place(ctx, "dummy", wid, wf)
	if err != context.DeadlineExceeded {
		t.Fatalf("place() error = %v, want DeadlineExceeded", err)
	}
}

func TestRankCandidatesFiltersRequiredLabelsWithANDSemantics(t *testing.T) {
	m := New(slog.New(slog.NewTextHandler(io.Discard, nil)), Config{})
	addCandidateSession(t, m, "linux-high", []string{"linux"}, 0.0, nil)
	addCandidateSession(t, m, "linux-arm", []string{"linux", "arm64"}, 0.25, nil)
	addCandidateSession(t, m, "unlabeled", nil, 0.5, nil)
	addCandidateSession(t, m, "linux-arm-gpu", []string{"linux", "arm64", "gpu"}, 0.75, nil)
	addCandidateSession(t, m, "linux-arm-full", []string{"linux", "arm64"}, 1.0, nil)

	tests := []struct {
		name           string
		requiredLabels []string
		want           []string
	}{
		{
			name: "no required labels keeps old capacity ranking",
			want: []string{"linux-high", "linux-arm", "unlabeled", "linux-arm-gpu"},
		},
		{
			name:           "single required label includes every candidate carrying it",
			requiredLabels: []string{"linux"},
			want:           []string{"linux-high", "linux-arm", "linux-arm-gpu"},
		},
		{
			name:           "all required labels must be present",
			requiredLabels: []string{"linux", "arm64"},
			want:           []string{"linux-arm", "linux-arm-gpu"},
		},
		{
			name:           "one missing required label excludes the candidate",
			requiredLabels: []string{"linux", "arm64", "gpu"},
			want:           []string{"linux-arm-gpu"},
		},
		{
			name:           "unknown required label leaves no candidate",
			requiredLabels: []string{"linux", "arm64", "metal"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			assertRankedNodes(t, m.rankCandidates("dummy", tt.requiredLabels), tt.want)
		})
	}
}

func TestPlaceWithMissingRequiredLabelsStaysPendingWithoutReserve(t *testing.T) {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	m := New(l, Config{BidTimeout: 10 * time.Millisecond})
	reserveSent := make(chan struct{}, 1)
	addCandidateSession(t, m, "linux-only", []string{"linux"}, 0.75, scriptedEncoder(func(msg *millproto.Message) error {
		if msg.GetReserveSeat() != nil {
			select {
			case reserveSent <- struct{}{}:
			default:
			}
		}
		return nil
	}))
	wf := testWorkflowWithRunsOn("build", []string{"linux", "arm64"})
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}

	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Millisecond)
	defer cancel()
	_, err := m.place(ctx, "dummy", wid, wf)
	if err != context.DeadlineExceeded {
		t.Fatalf("place() error = %v, want DeadlineExceeded while job remains pending", err)
	}
	select {
	case <-reserveSent:
		t.Fatal("place() sent ReserveSeat to executor missing a required label")
	default:
	}
}

func TestMaxPendingRejects(t *testing.T) {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	m := New(l, Config{MaxPending: 1})

	m.mu.Lock()
	m.pending = 1
	m.mu.Unlock()

	wf2 := testWorkflow("b")
	_, err := m.place(context.Background(), "dummy", models.WorkflowId{Name: "b"}, wf2)
	if err == nil {
		t.Fatal("place() past maxPending should error")
	}
}

func TestCancelledRunningLeaseSurvivesReleaseForReconnectReplay(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = wid
	lease.setState(leaseRunning)
	if err := m.persistLease(lease, leaseRowRunning); err != nil {
		t.Fatalf("persistLease: %v", err)
	}
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	m.destroy(wid)
	slot := &millSlot{fleet: m, lease: lease}
	slot.Release()

	m.mu.Lock()
	_, retained := m.leases[lease.id]
	m.mu.Unlock()
	if !retained {
		t.Fatal("slot release removed a cancellation-requested running lease before its terminal result")
	}
	if rows, err := bdb.ListMillLeases(); err != nil || len(rows) != 1 {
		t.Fatalf("durable leases after slot release = %+v, err = %v; want retained lease", rows, err)
	}

	var sentMu sync.Mutex
	var sent []*millproto.Message
	sess := newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		sentMu.Lock()
		sent = append(sent, msg)
		sentMu.Unlock()
		return nil
	}), discardLogger())
	if _, ok := m.attachSession(sess); !ok {
		t.Fatal("attachSession rejected reconnect")
	}
	m.sessionReady(sess)
	sentMu.Lock()
	var replayed bool
	for _, msg := range sent {
		if cancel := msg.GetCancelAttempt(); cancel != nil && cancel.GetLeaseId() == lease.id {
			replayed = true
		}
	}
	sentMu.Unlock()
	if !replayed {
		t.Fatal("reconnect did not replay CancelAttempt for retained lease")
	}

	if err := m.onEventBatch(sess, &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: lease.id,
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_CANCELLED,
					},
				},
			},
		},
	}); err != nil {
		t.Fatalf("onEventBatch: %v", err)
	}
	m.mu.Lock()
	_, retained = m.leases[lease.id]
	m.mu.Unlock()
	if retained {
		t.Fatal("terminal result did not clean retained cancelled lease")
	}
	if rows, err := bdb.ListMillLeases(); err != nil || len(rows) != 0 {
		t.Fatalf("durable leases after terminal = %+v, err = %v; want none", rows, err)
	}
	slot.Release()
}

func TestSessionRequestCancelledBeforeRegistrationOrSend(t *testing.T) {
	sent := 0
	sess := newSession("node-1", "inc-1", nil, scriptedEncoder(func(*millproto.Message) error {
		sent++
		return nil
	}), discardLogger())
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	_, err := sess.request(ctx, "lease-1", &millproto.Message{ReleaseLease: &millv1.ReleaseLease{LeaseId: "lease-1"}})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("request error = %v, want context.Canceled", err)
	}
	if sent != 0 {
		t.Fatalf("request sent %d messages for an already-cancelled context, want 0", sent)
	}
	sess.mu.Lock()
	pending := len(sess.pending)
	sess.mu.Unlock()
	if pending != 0 {
		t.Fatalf("request left %d pending waiters, want 0", pending)
	}
}

func TestAttachSessionReplacesSilentIncumbentButRejectsActiveDuplicate(t *testing.T) {
	m := New(discardLogger(), Config{ReconnectGrace: time.Minute})
	old := newSession("node-1", "inc-old", nil, nopEncoder(), discardLogger())
	transportClosed := make(chan struct{})
	old.closeTransport = func() error {
		close(transportClosed)
		return nil
	}
	if _, ok := m.attachSession(old); !ok {
		t.Fatal("first attach rejected")
	}
	m.mu.Lock()
	old.lastSeen = time.Now().Add(-2 * m.cfg.ReconnectGrace)
	m.mu.Unlock()

	replacement := newSession("node-1", "inc-new", nil, nopEncoder(), discardLogger())
	if _, ok := m.attachSession(replacement); !ok {
		t.Fatal("silent incumbent blocked authenticated replacement")
	}
	select {
	case <-transportClosed:
	default:
		t.Fatal("replacing a silent incumbent did not close its transport")
	}
	m.mu.Lock()
	replacement.lastSeen = time.Now().Add(-2 * m.cfg.ReconnectGrace)
	m.mu.Unlock()
	if err := replacement.dispatch(m, &millproto.Message{NodeSnapshot: &millv1.NodeSnapshot{Seqno: 1}}); err != nil {
		t.Fatalf("periodic snapshot dispatch: %v", err)
	}
	if _, ok := m.attachSession(newSession("node-1", "inc-dup", nil, nopEncoder(), discardLogger())); ok {
		t.Fatal("active replacement did not reject a duplicate session")
	}
}

func TestPlaceReleasesRemoteReservationWhenInitialPersistenceFails(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{BidTimeout: time.Second})
	if err := bdb.Close(); err != nil {
		t.Fatalf("close db: %v", err)
	}
	released := make(chan string, 1)
	var sess *millSession
	sess = addCandidateSession(t, m, "node-1", nil, 0, scriptedEncoder(func(msg *millproto.Message) error {
		switch {
		case msg.GetReserveSeat() != nil:
			leaseID := msg.GetReserveSeat().GetLeaseId()
			sess.deliver(leaseID, &millproto.Message{ReserveResult: &millv1.ReserveResult{
				LeaseId:  leaseID,
				Accepted: true,
			}})
		case msg.GetReleaseLease() != nil:
			released <- msg.GetReleaseLease().GetLeaseId()
		}
		return nil
	}))

	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	slot, err := m.place(
		ctx,
		"dummy",
		models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"},
		testWorkflow("build"),
	)
	if err == nil {
		t.Fatal("place succeeded after reserved lease persistence failed")
	}
	if slot != nil {
		t.Fatalf("place returned slot %T after persistence failure", slot)
	}
	select {
	case leaseID := <-released:
		if leaseID == "" {
			t.Fatal("ReleaseLease had empty lease id")
		}
	case <-time.After(time.Second):
		t.Fatal("persistence failure did not compensate with ReleaseLease")
	}
	m.mu.Lock()
	leases := len(m.leases)
	m.mu.Unlock()
	if leases != 0 {
		t.Fatalf("mill published %d leases after initial persistence failure, want 0", leases)
	}
}

func TestGapsAndDuplicates(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
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
				Seqno:   0,
				LeaseId: owned.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{Status: millv1.NonterminalStatus_RUNNING},
				},
			},
		},
	})
	if err != nil {
		t.Fatalf("expected duplicate to be skipped without error, got: %v", err)
	}

	err = m.onEventBatch(sess, &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   2,
				LeaseId: owned.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{Status: millv1.NonterminalStatus_RUNNING},
				},
			},
		},
	})
	if err == nil {
		t.Fatal("expected error due to seqno gap, got nil")
	}
}

func TestAtomicBatchRollback(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)

	owned := newLease("lease-1", "node-1", "inc-1", "dummy")
	owned.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	m.mu.Lock()
	m.leases[owned.id] = owned
	m.mu.Unlock()

	if _, err := bdb.Exec(`
		create trigger reject_status_event
		before insert on events
		begin
			select raise(abort, 'forced status event failure');
		end
	`); err != nil {
		t.Fatalf("failed to create fail trigger: %v", err)
	}
	defer bdb.Exec("drop trigger reject_status_event")

	err := m.onEventBatch(sess, &millv1.EventBatch{
		Epoch: sess.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: owned.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{Status: millv1.NonterminalStatus_RUNNING},
				},
			},
		},
	})
	if err == nil {
		t.Fatal("expected status event insertion to fail due to trigger")
	}

	m.mu.Lock()
	seqno := m.nodeSeqno["node-1/inc-1"]
	m.mu.Unlock()
	if seqno != 0 {
		t.Fatalf("expected seqno 0 due to rollback, got %d", seqno)
	}
}

func TestTerminalBeforeACK(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})

	ackSent := make(chan struct{})
	sess := newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		if msg.GetAck() != nil {
			wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
			st, err := bdb.GetStatus(wid)
			if err != nil || st.Status != "success" {
				t.Errorf("expected terminal status success at ACK time, got status: %v, err: %v", st, err)
			}
			close(ackSent)
		}
		return nil
	}), discardLogger())
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
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{Status: millv1.TerminalStatus_SUCCESS},
				},
			},
		},
	})
	if err != nil {
		t.Fatalf("onEventBatch: %v", err)
	}

	select {
	case <-ackSent:
	case <-time.After(2 * time.Second):
		t.Fatal("ACK was not sent")
	}
}

func TestExecutorRestartEmptySnapshot(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})

	lease := newLease("lease-1", "node-1", "inc-old", "dummy")
	lease.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()
	if err := m.persistLease(lease, leaseRowRunning); err != nil {
		t.Fatalf("persistLease: %v", err)
	}

	sess := newSession("node-1", "inc-new", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)

	err := m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno:          1,
		ActiveLeaseIds: nil,
	})
	if err != nil {
		t.Fatalf("onSnapshot: %v", err)
	}

	m.mu.Lock()
	_, stillActive := m.leases["lease-1"]
	m.mu.Unlock()
	if stillActive {
		t.Fatal("expected old epoch lease to be reconciled and failed")
	}
}

func TestReplacementLostBeforeSnapshotFailsOldEpochLease(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	lease := newLease("lease-1", "node-1", "inc-old", "dummy")
	lease.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease.setState(leaseRunning)
	if err := m.persistLease(lease, leaseRowRunning); err != nil {
		t.Fatalf("persistLease: %v", err)
	}
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	replacement := newSession("node-1", "inc-new", nil, nopEncoder(), discardLogger())
	if _, ok := m.attachSession(replacement); !ok {
		t.Fatal("attachSession rejected replacement")
	}
	replacement.disconnected = true
	m.failLeasesAfterGrace(replacement)

	m.mu.Lock()
	_, stillActive := m.leases[lease.id]
	m.mu.Unlock()
	if stillActive {
		t.Fatal("replacement loss stranded old-epoch lease")
	}
}

func TestSeqRegression(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)

	err := m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno: 5,
	})
	if err != nil {
		t.Fatalf("first snapshot: %v", err)
	}

	err = m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno: 4,
	})
	if err == nil {
		t.Fatal("expected seqno regression to be rejected")
	}
}

func TestClaimedSweep(t *testing.T) {
	m, _ := restoreTestMill(t, Config{ReconnectGrace: time.Minute})

	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease.orphaned = true
	lease.claimed = false
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	m.attachSession(sess)
	err := m.onSnapshot(sess, &millv1.NodeSnapshot{
		Seqno:          1,
		ActiveLeaseIds: []string{"lease-1"},
	})
	if err != nil {
		t.Fatalf("onSnapshot: %v", err)
	}

	m.detachSession(sess)

	m.sweepUnclaimedOrphans()

	m.mu.Lock()
	_, stillRunning := m.leases["lease-1"]
	m.mu.Unlock()
	if !stillRunning {
		t.Fatal("claimed lease was incorrectly swept by startup sweep")
	}
}

func TestCancelDeadline(t *testing.T) {
	m, _ := restoreTestMill(t, Config{CancelTimeout: 50 * time.Millisecond})

	sessClosed := make(chan struct{})
	sess := newSession("node-1", "inc-1", nil, scriptedEncoder(func(msg *millproto.Message) error {
		return nil
	}), discardLogger())
	sess.closeTransport = func() error {
		close(sessClosed)
		return nil
	}
	m.attachSession(sess)

	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	lease.setState(leaseRunning)
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	m.destroy(lease.wid)

	select {
	case <-sessClosed:
	case <-time.After(1 * time.Second):
		t.Fatal("session was not closed after cancel deadline expiration")
	}
}

func TestCleanupRetry(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})

	lease := newLease("lease-1", "node-1", "inc-1", "dummy")
	lease.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"}
	if err := m.persistLease(lease, leaseRowRunning); err != nil {
		t.Fatalf("persist lease: %v", err)
	}
	m.mu.Lock()
	m.leases[lease.id] = lease
	m.mu.Unlock()

	if _, err := bdb.Exec(`
		create trigger reject_cleanup_delete
		before delete on mill_leases
		begin
			select raise(abort, 'forced delete failure');
		end
	`); err != nil {
		t.Fatalf("failed to create fail trigger: %v", err)
	}

	err := m.cleanupLease(lease)
	if err == nil {
		t.Fatal("expected cleanupLease to fail")
	}

	m.mu.Lock()
	_, stillRunning := m.leases["lease-1"]
	m.mu.Unlock()
	if !stillRunning {
		t.Fatal("lease was removed from memory despite cleanup failure")
	}

	if _, err := bdb.Exec("drop trigger reject_cleanup_delete"); err != nil {
		t.Fatalf("drop trigger: %v", err)
	}

	err = m.cleanupLease(lease)
	if err != nil {
		t.Fatalf("expected retry cleanup to succeed, got: %v", err)
	}

	m.mu.Lock()
	_, stillRunning = m.leases["lease-1"]
	m.mu.Unlock()
	if stillRunning {
		t.Fatal("lease still in memory after successful cleanup retry")
	}
}

func TestBoundedBidding(t *testing.T) {
	m := New(discardLogger(), Config{TopK: 2, BidTimeout: 10 * time.Millisecond})

	addCandidateSession(t, m, "node-1", nil, 0, nil)
	addCandidateSession(t, m, "node-2", nil, 0, nil)
	addCandidateSession(t, m, "node-3", nil, 0, nil)
	addCandidateSession(t, m, "node-4", nil, 0, nil)
	addCandidateSession(t, m, "node-5", nil, 0, nil)

	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	lease, err := m.bid(ctx, "dummy", models.WorkflowId{}, testWorkflow("build"))
	if err != nil {
		t.Fatalf("bid: %v", err)
	}
	if lease != nil {
		t.Fatalf("did not expect a lease, got %+v", lease)
	}
}

func TestProtocolStrikesQuarantineExecutor(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute, QuarantineStrikes: 2})
	if err := bdb.AddExecutorToken("node-1", HashToken("tok-1"), nil, nil); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}
	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())

	resolve := func() bool {
		_, _, ok, err := bdb.ResolveExecutorToken(HashToken("tok-1"))
		if err != nil {
			t.Fatalf("ResolveExecutorToken: %v", err)
		}
		return ok
	}

	m.noteSessionError(sess, protoErrf("gap in stream seqnos. Expected 1, got 9"))
	if !resolve() {
		t.Fatal("quarantined on first strike, want threshold 2")
	}

	// a session that ends cleanly resets the count
	m.noteSessionError(sess, io.EOF)
	m.noteSessionError(sess, protoErrf("gap in stream seqnos. Expected 1, got 9"))
	if !resolve() {
		t.Fatal("transport death failed to reset the strike count")
	}

	m.noteSessionError(sess, protoErrf("gap in stream seqnos. Expected 1, got 9"))
	if resolve() {
		t.Fatal("expected quarantine after consecutive protocol deaths")
	}
	tokens, err := bdb.ListExecutorTokens()
	if err != nil || len(tokens) != 1 || tokens[0].QuarantineReason == nil {
		t.Fatalf("quarantine not visible in list: %+v, %v", tokens, err)
	}
}

func TestAttachSessionRereadsDatabaseCursor(t *testing.T) {
	m, bdb := restoreTestMill(t, Config{ReconnectGrace: time.Minute})
	if err := bdb.ApplyEventBatch(nil, func(tx *db.EventBatchTx) error {
		return tx.AdvanceCursor("node-1", "inc-1", 3)
	}); err != nil {
		t.Fatalf("AdvanceCursor: %v", err)
	}

	sess := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	if resume, ok := m.attachSession(sess); !ok || resume != 3 {
		t.Fatalf("attachSession resume = %d, %v; want 3 from the db cursor", resume, ok)
	}

	// an operator skip-forward must take effect on the next reconnect without
	// a mill restart
	if _, err := bdb.SetExecutorCursors("node-1", 28); err != nil {
		t.Fatalf("SetExecutorCursors: %v", err)
	}
	m.mu.Lock()
	sess.disconnected = true
	m.mu.Unlock()
	replacement := newSession("node-1", "inc-1", nil, nopEncoder(), discardLogger())
	if resume, ok := m.attachSession(replacement); !ok || resume != 28 {
		t.Fatalf("attachSession resume after reset = %d, %v; want 28", resume, ok)
	}
}
