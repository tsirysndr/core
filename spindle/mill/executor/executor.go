package executor

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"net/http"
	"runtime"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"log/slog"
	"strings"

	"tangled.org/core/api/tangled"
	"tangled.org/core/netutil"
	"tangled.org/core/notifier"
	"tangled.org/core/spindle/artifactstore"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
)

const (
	dialBackoffMin = 1 * time.Second
	dialBackoffMax = 30 * time.Second
	snapshotEvery  = 15 * time.Second
	defaultSeats   = 4
)

type Executor struct {
	millURL string
	token   string
	nodeID  string
	seats   int
	labels  []string

	engines map[string]models.Engine
	db      *db.DB
	n       *notifier.Notifier
	cfg     *config.Config
	l       *slog.Logger
	writer  artifactstore.Writer

	epoch          string
	outboxBytes    int64
	maxOutboxBytes int64 // 10 MiB default outbox cap

	eventMu sync.Mutex
	sendMu  sync.Mutex
	flushMu sync.Mutex

	sentSeqno uint64

	connMu        sync.Mutex
	enc           messageEncoder
	sessionCancel context.CancelFunc

	mu       sync.Mutex
	active   map[string]*reservation
	draining bool

	snapshotMu sync.Mutex
	nextSeqno  uint64

	lifecycleCtx context.Context
	jobsWG       sync.WaitGroup
}

type reservation struct {
	leaseID    string
	wid        models.WorkflowId
	realEngine models.Engine
	slot       engine.WorkflowSlot
	wf         *models.Workflow
	repoDid    syntax.DID
	vault      *memVault

	committed bool
	cancelled bool
	cancel    context.CancelFunc
	ttlTimer  *time.Timer
	stopTail  func()
}

type messageEncoder interface {
	Encode(*millproto.Message) error
}

func New(cfg *config.Config, engines map[string]models.Engine, d *db.DB, n *notifier.Notifier, l *slog.Logger, writers ...artifactstore.Writer) (*Executor, error) {
	seats := defaultSeats
	millURL := ""
	token := ""
	nodeID := ""
	var labels []string
	if cfg != nil {
		if cfg.Mill.Seats > 0 {
			seats = cfg.Mill.Seats
		}
		labels = normalizeLabels(cfg.Mill.Labels)
		millURL = cfg.Mill.URL
		token = cfg.Mill.SharedSecret
		nodeID = cfg.Server.Hostname
	}
	if d == nil || n == nil {
		return nil, fmt.Errorf("executor requires a database and notifier")
	}
	var writer artifactstore.Writer
	if len(writers) > 0 {
		writer = writers[0]
	}
	e := &Executor{
		millURL:        millURL,
		token:          token,
		nodeID:         nodeID,
		seats:          seats,
		labels:         labels,
		engines:        engines,
		db:             d,
		n:              n,
		cfg:            cfg,
		l:              l.With("component", "mill.executor"),
		writer:         writer,
		active:         make(map[string]*reservation),
		maxOutboxBytes: 10 * 1024 * 1024,
	}
	if err := e.initOutbox(); err != nil {
		return nil, fmt.Errorf("initialize executor outbox: %w", err)
	}
	return e, nil
}

func (e *Executor) Connect(ctx context.Context) {
	e.lifecycleCtx = ctx
	sub := e.n.Subscribe()
	cursor, err := e.db.EventHighWater()
	if err != nil {
		e.n.Unsubscribe(sub)
		e.l.Error("establish event cursor failed", "err", err)
		return
	}
	e.drainEvents(&cursor)
	observerCtx, stopObserver := context.WithCancel(ctx)
	observerDone := make(chan struct{})
	go func() {
		defer close(observerDone)
		e.observeLoop(observerCtx, sub, cursor)
	}()
	defer e.n.Unsubscribe(sub)

	backoff := dialBackoffMin
	for {
		if ctx.Err() != nil {
			break
		}
		err := e.runSession(ctx)
		if ctx.Err() != nil {
			break
		}
		e.l.Warn("mill session ended; reconnecting", "err", err, "backoff", backoff)
		select {
		case <-ctx.Done():
			break
		case <-time.After(backoff):
		}
		backoff = min(backoff*2, dialBackoffMax)
	}

	e.jobsWG.Wait()
	stopObserver()
	<-observerDone
	e.drainEvents(&cursor)
}

func (e *Executor) runSession(ctx context.Context) error {
	dev := e.cfg == nil || e.cfg.Server.Dev
	if _, err := netutil.EnforceWSSURL(e.millURL, dev); err != nil {
		return fmt.Errorf("mill url: %w", err)
	}
	header := http.Header{}
	if e.token != "" {
		header.Set("Authorization", "Bearer "+e.token)
	}
	conn, _, err := netutil.SSRFWebsocketDialer(dev).DialContext(ctx, e.millURL, header)
	if err != nil {
		return fmt.Errorf("dial mill: %w", err)
	}
	defer conn.Close()

	sessionCtx, cancelSession := context.WithCancel(ctx)
	defer cancelSession()
	stopClose := context.AfterFunc(sessionCtx, func() { _ = conn.Close() })
	defer stopClose()

	stream := millproto.NewWSStream(conn)
	enc := millproto.NewEncoder(stream)
	dec := millproto.NewDecoder(stream)

	hello := &millproto.Message{Hello: &millv1.Hello{
		ProtocolVersion: millproto.ProtocolVersion,
		Arch:            runtime.GOARCH,
		Labels:          e.labels,
		Epoch:           e.epoch,
	}}
	if err := enc.Encode(hello); err != nil {
		return fmt.Errorf("send hello: %w", err)
	}

	resumeMsg, err := dec.Decode()
	if err != nil {
		return fmt.Errorf("read resume: %w", err)
	}
	resume := resumeMsg.GetResume()
	if resume == nil {
		return fmt.Errorf("expected resume, got something else")
	}
	if resume.GetEpoch() != e.epoch {
		return fmt.Errorf("resume epoch mismatch: got %q, want %q", resume.GetEpoch(), e.epoch)
	}

	e.connMu.Lock()
	e.sessionCancel = cancelSession
	e.enc = enc
	e.connMu.Unlock()
	defer func() {
		e.connMu.Lock()
		e.enc = nil
		e.sessionCancel = nil
		e.connMu.Unlock()
	}()

	readErr := make(chan error, 1)
	go func() {
		for {
			msg, err := dec.Decode()
			if err != nil {
				readErr <- fmt.Errorf("read: %w", err)
				return
			}
			e.dispatch(sessionCtx, msg)
		}
	}()

	if err := e.replay(resume.GetAckSeqno()); err != nil {
		cancelSession()
		<-readErr
		return fmt.Errorf("replay failed: %w", err)
	}
	e.pushSnapshot()
	e.l.Info("connected to mill", "node", e.nodeID, "resumeFrom", resume.GetAckSeqno())

	go e.snapshotLoop(sessionCtx, enc)
	return <-readErr
}
func (e *Executor) send(msg *millproto.Message) {
	e.connMu.Lock()
	enc := e.enc
	cancel := e.sessionCancel
	e.connMu.Unlock()
	if enc != nil {
		e.sendMu.Lock()
		err := enc.Encode(msg)
		e.sendMu.Unlock()
		if err != nil {
			e.l.Error("send failed, ending session", "err", err)
			if cancel != nil {
				cancel()
			}
		}
	}
}

func (e *Executor) dispatch(ctx context.Context, msg *millproto.Message) {
	switch {
	case msg.GetReserveSeat() != nil:
		e.handleReserve(ctx, msg.GetReserveSeat())
	case msg.GetCommitLease() != nil:
		e.handleCommit(ctx, msg.GetCommitLease())
	case msg.GetReleaseLease() != nil:
		e.handleRelease(msg.GetReleaseLease().GetLeaseId())
	case msg.GetCancelAttempt() != nil:
		e.handleCancel(msg.GetCancelAttempt().GetLeaseId())
	case msg.GetAck() != nil:
		e.handleAck(msg.GetAck())
	default:
		e.l.Warn("unhandled incoming message", "type", fmt.Sprintf("%T", msg))
	}
}

func (e *Executor) sendReject(leaseID string, reason string, class millv1.RejectClass) {
	e.send(&millproto.Message{ReserveResult: &millv1.ReserveResult{
		LeaseId:      leaseID,
		Accepted:     false,
		RejectReason: reason,
		RejectClass:  class,
	}})
}

func (e *Executor) sendCommitted(leaseID string) {
	e.send(&millproto.Message{Committed: &millv1.Committed{LeaseId: leaseID}})
}

func (e *Executor) sendCancelAck(leaseID string) {
	e.send(&millproto.Message{CancelAck: &millv1.CancelAck{LeaseId: leaseID}})
}

func (e *Executor) releaseReservation(cleanup func()) {
	if cleanup != nil {
		cleanup()
	}
	e.pushSnapshot()
}

func (e *Executor) handleReserve(ctx context.Context, rs *millv1.ReserveSeat) {
	reject := func(reason string, class millv1.RejectClass) {
		e.sendReject(rs.GetLeaseId(), reason, class)
	}

	e.mu.Lock()
	draining := e.draining
	e.mu.Unlock()
	if draining {
		reject("draining", millv1.RejectClass_REJECT_CLASS_TRANSIENT)
		return
	}

	realEngine, ok := e.engines[rs.GetTargetEngine()]
	if !ok {
		reject("unknown engine "+rs.GetTargetEngine(), millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}
	slotter, ok := realEngine.(engine.WorkflowSlotter)
	if !ok {
		reject("engine does not support workflow slots", millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}

	var twf tangled.Pipeline_Workflow
	if err := json.Unmarshal([]byte(rs.GetRawWorkflowJson()), &twf); err != nil {
		reject("bad workflow json", millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}
	var tpl tangled.Pipeline
	if err := json.Unmarshal([]byte(rs.GetRawPipelineJson()), &tpl); err != nil {
		reject("bad pipeline json", millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}
	if tpl.TriggerMetadata == nil {
		reject("pipeline missing trigger metadata", millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}

	pipelineId := models.PipelineId{Knot: rs.GetKnot(), Rkey: rs.GetRkey()}
	wid := models.WorkflowId{PipelineId: pipelineId, Name: twf.Name}

	wf, err := realEngine.InitWorkflow(twf, tpl)
	if err != nil {
		reject("init workflow: "+err.Error(), millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
		return
	}
	if validator, ok := realEngine.(engine.WorkflowPlacementValidator); ok {
		if err := validator.ValidateWorkflowPlacement(wf); err != nil {
			reject("validate workflow placement: "+err.Error(), millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE)
			return
		}
	}
	if wf.Environment == nil {
		wf.Environment = make(map[string]string)
	}
	maps.Copy(wf.Environment, models.PipelineEnvVars(tpl.TriggerMetadata, pipelineId))

	slot, err := slotter.AcquireWorkflowSlot(ctx, wid, wf, engine.NoWait)
	if err != nil {
		class := millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE
		if errors.Is(err, engine.ErrNoWorkflowSlots) {
			class = millv1.RejectClass_REJECT_CLASS_TRANSIENT
		}
		reject(err.Error(), class)
		return
	}

	var repoDid syntax.DID
	if tpl.TriggerMetadata != nil && tpl.TriggerMetadata.Repo != nil && tpl.TriggerMetadata.Repo.RepoDid != nil {
		repoDid, _ = syntax.ParseDID(*tpl.TriggerMetadata.Repo.RepoDid)
	}

	res := &reservation{
		leaseID:    rs.GetLeaseId(),
		wid:        wid,
		realEngine: realEngine,
		slot:       slot,
		wf:         wf,
		repoDid:    repoDid,
	}

	e.snapshotMu.Lock()
	e.mu.Lock()
	e.active[res.leaseID] = res
	res.ttlTimer = time.AfterFunc(ttlDuration(rs.GetTtlSeconds()), func() { e.expireReservation(res.leaseID) })
	e.mu.Unlock()

	e.send(&millproto.Message{ReserveResult: &millv1.ReserveResult{
		LeaseId:  rs.GetLeaseId(),
		Accepted: true,
	}})
	e.pushSnapshotLocked()
	e.snapshotMu.Unlock()
}

func (e *Executor) handleCommit(ctx context.Context, cl *millv1.CommitLease) {
	e.mu.Lock()
	res := e.active[cl.GetLeaseId()]
	if res == nil {
		e.mu.Unlock()
		e.sendReject(cl.GetLeaseId(), "reservation missing or expired", millv1.RejectClass_REJECT_CLASS_TRANSIENT)
		return
	}
	if res.committed {
		e.mu.Unlock()
		e.sendCommitted(cl.GetLeaseId())
		return
	}
	res.committed = true
	if res.ttlTimer != nil {
		res.ttlTimer.Stop()
	}

	jobCtx, cancel := context.WithCancel(e.lifecycleCtx)
	res.cancel = cancel
	e.mu.Unlock()

	vault := newMemVault(cl.GetSecrets())
	re := newReservedEngine(res.realEngine, res.slot)
	pipeline := &models.Pipeline{
		RepoDid:       res.repoDid,
		Workflows:     map[models.Engine][]models.Workflow{re: {*res.wf}},
		TrustedSource: true,
	}

	e.startTail(res)

	e.jobsWG.Add(1)
	go func() {
		defer e.jobsWG.Done()
		engine.StartWorkflows(e.l, vault, e.cfg, nil, e.db, e.n, jobCtx, pipeline, res.wid.PipelineId)
	}()

	e.sendCommitted(cl.GetLeaseId())
}

func (e *Executor) handleRelease(leaseID string) {
	cleanup, ok := e.takeUncommittedReservation(leaseID, true)
	if !ok {
		return
	}
	e.releaseReservation(cleanup)
}

func (e *Executor) handleCancel(leaseID string) {
	e.mu.Lock()
	res := e.active[leaseID]
	if res == nil {
		e.mu.Unlock()
		if err := e.appendTerminal(leaseID, string(models.StatusKindCancelled), nil); err != nil {
			e.l.Error("persist cancelled reservation terminal", "lease", leaseID, "err", err)
			return
		}
		e.sendCancelAck(leaseID)
		return
	}
	res.cancelled = true
	cancel := res.cancel
	committed := res.committed
	var cleanup func()
	if !committed {
		cleanup = e.removeReservationLocked(res, true)
	}
	e.mu.Unlock()

	if !committed {
		if err := e.appendTerminal(leaseID, string(models.StatusKindCancelled), nil); err != nil {
			e.l.Error("persist cancelled reservation terminal", "lease", leaseID, "err", err)
			e.releaseReservation(cleanup)
			return
		}
		e.sendCancelAck(leaseID)
		e.releaseReservation(cleanup)
		return
	}

	e.sendCancelAck(leaseID)
	if cancel != nil {
		cancel()
	}
}

func (e *Executor) expireReservation(leaseID string) {
	cleanup, ok := e.takeUncommittedReservation(leaseID, true)
	if !ok {
		return
	}
	e.releaseReservation(cleanup)
}

func (e *Executor) takeUncommittedReservation(leaseID string, releaseSlot bool) (func(), bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	res := e.active[leaseID]
	if res == nil || res.committed {
		return nil, false
	}
	if res.ttlTimer != nil {
		res.ttlTimer.Stop()
	}
	return e.removeReservationLocked(res, releaseSlot), true
}

func (e *Executor) removeReservationLocked(res *reservation, releaseSlot bool) func() {
	delete(e.active, res.leaseID)
	slot := res.slot
	return func() {
		if releaseSlot && slot != nil {
			slot.Release()
		}
	}
}

func (e *Executor) snapshotLoop(ctx context.Context, enc *millproto.Encoder) {
	ticker := time.NewTicker(snapshotEvery)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			e.pushSnapshot()
		}
	}
}

func (e *Executor) pushSnapshot() {
	e.snapshotMu.Lock()
	defer e.snapshotMu.Unlock()
	e.pushSnapshotLocked()
}

func (e *Executor) pushSnapshotLocked() {
	e.mu.Lock()
	activeLeases := make([]string, 0, len(e.active))
	for leaseID := range e.active {
		activeLeases = append(activeLeases, leaseID)
	}
	e.mu.Unlock()

	avail := make(map[string]*millv1.EngineAvailability)
	for name, eng := range e.engines {
		a := &millv1.EngineAvailability{Available: true}
		if getter, ok := eng.(interface{ Load() map[string]float64 }); ok {
			a.Load = getter.Load()
		}
		avail[name] = a
	}

	e.nextSeqno++
	snap := &millproto.Message{
		NodeSnapshot: &millv1.NodeSnapshot{
			Seqno:          e.nextSeqno,
			Engines:        avail,
			ActiveLeaseIds: activeLeases,
		},
	}
	e.send(snap)
}

func (e *Executor) Drain() {
	e.mu.Lock()
	e.draining = true
	e.mu.Unlock()
	e.pushSnapshot()
}

func ttlDuration(secs uint32) time.Duration {
	if secs == 0 {
		return defaultReservationTTL
	}
	return time.Duration(secs) * time.Second
}

const defaultReservationTTL = 60 * time.Second

func normalizeLabels(labels []string) []string {
	seen := make(map[string]struct{}, len(labels))
	out := make([]string, 0, len(labels))
	for _, label := range labels {
		label = strings.TrimSpace(label)
		if label == "" {
			continue
		}
		if _, ok := seen[label]; ok {
			continue
		}
		seen[label] = struct{}{}
		out = append(out, label)
	}
	return out
}
