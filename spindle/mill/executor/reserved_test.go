package executor

import (
	"context"
	"encoding/json"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/gorilla/websocket"
	"google.golang.org/protobuf/proto"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/notifier"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

type captureEncoder struct {
	messages chan *millproto.Message
}

func newCaptureEncoder() *captureEncoder {
	return &captureEncoder{messages: make(chan *millproto.Message, 4)}
}

func (e *captureEncoder) Encode(msg *millproto.Message) error {
	e.messages <- msg
	return nil
}

type fakeSlot struct{ released int }

func (s *fakeSlot) Release() { s.released++ }

type fakeEngine struct {
	setupCalled   bool
	runCalled     bool
	destroyCalled bool
	acquireCalled bool
	secrets       chan []secrets.UnlockedSecret
	done          chan struct{}
}

func (e *fakeEngine) InitWorkflow(twf tangled.Pipeline_Workflow, tpl tangled.Pipeline) (*models.Workflow, error) {
	return &models.Workflow{Name: twf.Name}, nil
}
func (e *fakeEngine) SetupWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, l models.WorkflowLogger) error {
	e.setupCalled = true
	return nil
}
func (e *fakeEngine) WorkflowTimeout() time.Duration { return 7 * time.Minute }
func (e *fakeEngine) DestroyWorkflow(ctx context.Context, wid models.WorkflowId) error {
	e.destroyCalled = true
	return nil
}
func (e *fakeEngine) RunStep(ctx context.Context, wid models.WorkflowId, w *models.Workflow, idx int, s []secrets.UnlockedSecret, l models.WorkflowLogger) error {
	e.runCalled = true
	if e.secrets != nil {
		e.secrets <- s
	}
	if e.done != nil {
		close(e.done)
	}
	return nil
}
func (e *fakeEngine) AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, mode engine.AcquireMode) (engine.WorkflowSlot, error) {
	e.acquireCalled = true
	return &fakeSlot{}, nil
}

type fakeStep struct{}

func (fakeStep) Name() string          { return "test" }
func (fakeStep) Command() string       { return "true" }
func (fakeStep) Kind() models.StepKind { return models.StepKindUser }

func TestNewFailsWhenOutboxCannotInitialize(t *testing.T) {
	d := testDB(t)
	if err := d.Close(); err != nil {
		t.Fatal(err)
	}
	n := notifier.New()
	cfg := &config.Config{}
	if _, err := New(cfg, nil, d, &n, slog.New(slog.NewTextHandler(io.Discard, nil))); err == nil {
		t.Fatal("New succeeded with an unavailable outbox database")
	}
}

func TestReservedEngineHandsBackHeldSlotOnce(t *testing.T) {
	inner := &fakeEngine{}
	slot := &fakeSlot{}
	re := newReservedEngine(inner, slot)

	got, err := re.(engine.WorkflowSlotter).AcquireWorkflowSlot(context.Background(), models.WorkflowId{}, nil, engine.Wait)
	if err != nil {
		t.Fatal(err)
	}
	if got != engine.WorkflowSlot(slot) {
		t.Fatal("AcquireWorkflowSlot() did not return the held slot")
	}
	if inner.acquireCalled {
		t.Fatal("wrapper must not call the inner engine's AcquireWorkflowSlot")
	}

	if _, err := re.(engine.WorkflowSlotter).AcquireWorkflowSlot(context.Background(), models.WorkflowId{}, nil, engine.Wait); err == nil {
		t.Fatal("second AcquireWorkflowSlot() should error")
	}
}

func TestHandleCommitIsIdempotent(t *testing.T) {
	enc := newCaptureEncoder()
	e := testExecutor(t)
	e.enc = enc
	e.active["lease-1"] = &reservation{leaseID: "lease-1", committed: true}

	e.handleCommit(context.Background(), &millv1.CommitLease{LeaseId: "lease-1"})
	msg := <-enc.messages
	if got := msg.GetCommitted().GetLeaseId(); got != "lease-1" {
		t.Fatalf("Committed lease = %q, want lease-1", got)
	}
}

func TestHandleCommitRejectsMissingReservation(t *testing.T) {
	enc := newCaptureEncoder()
	e := testExecutor(t)
	e.enc = enc

	e.handleCommit(context.Background(), &millv1.CommitLease{LeaseId: "expired"})
	result := (<-enc.messages).GetReserveResult()
	if result == nil {
		t.Fatal("missing reservation commit did not receive a ReserveResult")
	}
	if result.GetLeaseId() != "expired" || result.GetAccepted() {
		t.Fatalf("ReserveResult = %+v, want correlated rejection", result)
	}
}

func TestHandleCancelFinalizesExpiredReservation(t *testing.T) {
	enc := newCaptureEncoder()
	e := testExecutor(t)
	e.enc = enc

	e.handleCancel("lease-expired")
	var ack *millv1.CancelAck
	for ack == nil {
		select {
		case msg := <-enc.messages:
			ack = msg.GetCancelAck()
		case <-time.After(time.Second):
			t.Fatal("cancel acknowledgement timed out")
		}
	}
	if ack.GetLeaseId() != "lease-expired" {
		t.Fatalf("CancelAck = %+v, want lease-expired", ack)
	}
	rows, err := e.db.ListOutboxRows()
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 {
		t.Fatalf("cancel terminal outbox rows = %d, want 1", len(rows))
	}
	var entry millv1.Event
	if err := proto.Unmarshal(rows[0].Payload, &entry); err != nil {
		t.Fatal(err)
	}
	if got := entry.GetAttemptResult().GetStatus(); got != millv1.TerminalStatus_CANCELLED {
		t.Fatalf("cancel terminal = %v, want CANCELLED", got)
	}
}

func TestHandleCommitPreservesPreauthorizedSecrets(t *testing.T) {
	d := testDB(t)
	n := notifier.New()
	enc := newCaptureEncoder()
	e := &Executor{
		cfg:            &config.Config{Server: config.Server{LogDir: t.TempDir()}},
		db:             d,
		n:              &n,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
		enc:            enc,
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}
	e.lifecycleCtx = context.Background()

	inner := &fakeEngine{secrets: make(chan []secrets.UnlockedSecret, 1), done: make(chan struct{})}
	slot := &fakeSlot{}
	repoDid, err := syntax.ParseDID("did:web:example.com")
	if err != nil {
		t.Fatal(err)
	}
	res := &reservation{
		leaseID:    "lease-1",
		wid:        models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"},
		realEngine: inner,
		slot:       slot,
		wf:         &models.Workflow{Name: "build", Steps: []models.Step{fakeStep{}}},
		repoDid:    repoDid,
	}
	e.active[res.leaseID] = res

	e.handleCommit(context.Background(), &millv1.CommitLease{
		LeaseId: res.leaseID,
		Secrets: []*millv1.Secret{{Key: "TOKEN", Value: "secret-value"}},
	})
	if got := (<-enc.messages).GetCommitted().GetLeaseId(); got != res.leaseID {
		t.Fatalf("Committed lease = %q, want %q", got, res.leaseID)
	}
	select {
	case got := <-inner.secrets:
		if len(got) != 1 || got[0].Key != "TOKEN" || got[0].Value != "secret-value" {
			t.Fatalf("RunStep secrets = %+v", got)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("RunStep did not receive CommitLease secrets")
	}
	select {
	case <-inner.done:
	case <-time.After(2 * time.Second):
		t.Fatal("workflow did not finish")
	}
	if res.stopTail != nil {
		res.stopTail()
	}
	e.jobsWG.Wait()
}

func TestRunSessionCancellationClosesStalledWebsocket(t *testing.T) {
	connected := make(chan struct{})
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Upgrade(w, r, nil, 1024, 1024)
		if err != nil {
			return
		}
		defer conn.Close()
		close(connected)
		<-release
	}))
	t.Cleanup(func() {
		close(release)
		srv.Close()
	})

	e := testSessionExecutor(t, "ws"+strings.TrimPrefix(srv.URL, "http"))
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- e.runSession(ctx) }()
	<-connected
	cancel()

	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("runSession did not return after context cancellation")
	}
}

func testSessionExecutor(t *testing.T, url string) *Executor {
	d := testDB(t)
	e := &Executor{
		millURL:        url,
		seats:          1,
		engines:        make(map[string]models.Engine),
		db:             d,
		cfg:            &config.Config{Server: config.Server{Dev: true}},
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}
	return e
}

func testExecutor(t *testing.T) *Executor {
	d := testDB(t)
	e := &Executor{
		db:             d,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}
	return e
}

func testDB(t *testing.T) *db.DB {
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatal(err)
	}
	return d
}

func TestFinishJobReportsCancelledReservationAsCancelled(t *testing.T) {
	d := testDB(t)
	res := &reservation{
		leaseID:   "lease-1",
		wid:       models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"},
		cancelled: true,
	}
	e := &Executor{
		db:             d,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         map[string]*reservation{res.leaseID: res},
		maxOutboxBytes: 10 * 1024 * 1024,
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}

	e.finishJob(res, &tangled.PipelineStatus{
		Pipeline: string(res.wid.PipelineId.AtUri()),
		Workflow: res.wid.Name,
		Status:   string(models.StatusKindFailed),
	})

	rows, err := d.ListOutboxRows()
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 {
		t.Fatalf("outbox rows = %d, want 1", len(rows))
	}

	var entry millv1.Event
	if err := proto.Unmarshal(rows[0].Payload, &entry); err != nil {
		t.Fatal(err)
	}
	got := entry.GetAttemptResult().GetStatus()
	if got != millv1.TerminalStatus_CANCELLED {
		t.Fatalf("terminal status = %v, want CANCELLED", got)
	}
}

func TestReplayRejectsMalformedOutboxRow(t *testing.T) {
	d := testDB(t)
	e := &Executor{
		db:  d,
		enc: newCaptureEncoder(),
		l:   slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}
	if _, err := d.AppendOutboxRow([]byte("not protobuf"), true); err != nil {
		t.Fatal(err)
	}
	if err := e.replay(0); err == nil {
		t.Fatal("replay accepted a malformed row and would leave a permanent seqno gap")
	}
}

func TestSocketCancellationIndependence(t *testing.T) {
	d := testDB(t)
	n := notifier.New()
	e := &Executor{
		db:             d,
		n:              &n,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
		cfg:            &config.Config{Server: config.Server{LogDir: t.TempDir()}},
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}

	lifecycleCtx, cancelLifecycle := context.WithCancel(context.Background())
	defer cancelLifecycle()
	e.lifecycleCtx = lifecycleCtx

	inner := &fakeEngine{secrets: make(chan []secrets.UnlockedSecret, 1), done: make(chan struct{})}
	slot := &fakeSlot{}
	repoDid, err := syntax.ParseDID("did:web:example.com")
	if err != nil {
		t.Fatal(err)
	}
	res := &reservation{
		leaseID:    "lease-1",
		wid:        models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"},
		realEngine: inner,
		slot:       slot,
		wf:         &models.Workflow{Name: "build", Steps: []models.Step{fakeStep{}}},
		repoDid:    repoDid,
	}
	e.active[res.leaseID] = res

	sessionCtx, cancelSession := context.WithCancel(lifecycleCtx)

	e.handleCommit(sessionCtx, &millv1.CommitLease{
		LeaseId: "lease-1",
	})

	cancelSession()

	// session disconnect must not cancel the running job
	select {
	case <-inner.done:
	case <-time.After(2 * time.Second):
		t.Fatal("workflow did not complete even though websocket session was cancelled")
	}

	e.jobsWG.Wait()
}

func TestMonotonicSnapshots(t *testing.T) {
	enc := newCaptureEncoder()
	e := &Executor{
		enc:            enc,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
	}

	e.pushSnapshot()
	msg1 := <-enc.messages
	seq1 := msg1.GetNodeSnapshot().GetSeqno()
	if seq1 != 1 {
		t.Fatalf("first seq = %d, want 1", seq1)
	}

	e.pushSnapshot()
	msg2 := <-enc.messages
	seq2 := msg2.GetNodeSnapshot().GetSeqno()
	if seq2 != 2 {
		t.Fatalf("second seq = %d, want 2", seq2)
	}
}

func TestTimerRace(t *testing.T) {
	d := testDB(t)
	e := &Executor{
		db:             d,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}

	twf, _ := json.Marshal(tangled.Pipeline_Workflow{Name: "build"})
	tpl, _ := json.Marshal(tangled.Pipeline{TriggerMetadata: &tangled.Pipeline_TriggerMetadata{}})

	inner := &fakeEngine{}
	e.engines = map[string]models.Engine{"microvm": inner}

	e.handleReserve(context.Background(), &millv1.ReserveSeat{
		LeaseId:         "lease-1",
		TargetEngine:    "microvm",
		RawWorkflowJson: string(twf),
		RawPipelineJson: string(tpl),
		TtlSeconds:      1,
	})

	e.mu.Lock()
	res := e.active["lease-1"]
	e.mu.Unlock()

	if res == nil {
		t.Fatal("reservation was not added")
	}

	deadline := time.Now().Add(5 * time.Second)
	for {
		e.mu.Lock()
		activeLen := len(e.active)
		e.mu.Unlock()
		if activeLen == 0 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("reservation was leaked and never expired")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestStructuredShutdown(t *testing.T) {
	d := testDB(t)
	n := notifier.New()
	e := &Executor{
		db:             d,
		n:              &n,
		l:              slog.New(slog.NewTextHandler(io.Discard, nil)),
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
		cfg:            &config.Config{Server: config.Server{LogDir: t.TempDir()}},
	}
	if err := e.initOutbox(); err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	e.lifecycleCtx = ctx

	inner := &fakeEngine{secrets: make(chan []secrets.UnlockedSecret, 1), done: make(chan struct{})}
	slot := &fakeSlot{}
	repoDid, err := syntax.ParseDID("did:web:example.com")
	if err != nil {
		t.Fatal(err)
	}
	res := &reservation{
		leaseID:    "lease-1",
		wid:        models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "r"}, Name: "build"},
		realEngine: inner,
		slot:       slot,
		wf:         &models.Workflow{Name: "build", Steps: []models.Step{fakeStep{}}},
		repoDid:    repoDid,
	}
	e.active[res.leaseID] = res

	e.handleCommit(ctx, &millv1.CommitLease{
		LeaseId: "lease-1",
	})

	cancel()

	e.jobsWG.Wait()

	select {
	case <-inner.done:
	default:
		t.Fatal("shutdown returned but running job did not finish")
	}
}
