package mill

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"tangled.org/core/notifier"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
	"tangled.org/core/tid"

	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

const (
	defaultReconnectGrace    = 45 * time.Second
	defaultJobTimeout        = 24 * time.Hour
	defaultBidTimeout        = 5 * time.Second
	defaultTopK              = 3
	defaultMaxPending        = 100
	defaultQuarantineStrikes = 3
)

// marks session-ending errors that are the executor's fault, enough of them
// in a row gets the node quarantined
var errProtocolViolation = errors.New("executor protocol violation")

func protoErrf(format string, args ...any) error {
	return fmt.Errorf("%w: %s", errProtocolViolation, fmt.Sprintf(format, args...))
}

type Config struct {
	// mill appends live-tailed executor lines here so logview can follow running remote jobs
	LogDir         string
	MaxPending     int
	ReconnectGrace time.Duration
	JobTimeout     time.Duration
	BidTimeout     time.Duration
	TopK           int
	CancelTimeout  time.Duration
	// how many protocol-violating session deaths in a row quarantine the node
	QuarantineStrikes int
}

type Mill struct {
	l   *slog.Logger
	cfg Config

	db *db.DB
	n  *notifier.Notifier

	mu           sync.Mutex
	sessions     map[string]*millSession
	leases       map[string]*RemoteLease
	reservations map[string]*RemoteLease
	nodeSeqno    map[string]uint64
	protoStrikes map[string]int
	pending      int
	changeCh     chan struct{} // closed and replaced to wake placement waiters

	leaseSeq uint64
}

func New(l *slog.Logger, cfg Config) *Mill {
	if cfg.ReconnectGrace <= 0 {
		cfg.ReconnectGrace = defaultReconnectGrace
	}
	if cfg.JobTimeout <= 0 {
		cfg.JobTimeout = defaultJobTimeout
	}
	if cfg.BidTimeout <= 0 {
		cfg.BidTimeout = defaultBidTimeout
	}
	if cfg.TopK <= 0 {
		cfg.TopK = defaultTopK
	}
	if cfg.MaxPending <= 0 {
		cfg.MaxPending = defaultMaxPending
	}
	if cfg.CancelTimeout <= 0 {
		cfg.CancelTimeout = 10 * time.Second
	}
	if cfg.QuarantineStrikes <= 0 {
		cfg.QuarantineStrikes = defaultQuarantineStrikes
	}
	return &Mill{
		l:            l,
		cfg:          cfg,
		sessions:     make(map[string]*millSession),
		leases:       make(map[string]*RemoteLease),
		reservations: make(map[string]*RemoteLease),
		nodeSeqno:    make(map[string]uint64),
		protoStrikes: make(map[string]int),
		changeCh:     make(chan struct{}),
	}
}

func (m *Mill) Attach(d *db.DB, n *notifier.Notifier) {
	m.mu.Lock()
	m.db = d
	m.n = n
	m.mu.Unlock()
}

func (m *Mill) nextLeaseID() string {
	m.mu.Lock()
	m.leaseSeq++
	seq := m.leaseSeq
	m.mu.Unlock()
	return fmt.Sprintf("%s-%d", tid.TID(), seq)
}

func (m *Mill) dropReservation(id string) {
	m.mu.Lock()
	delete(m.reservations, id)
	m.mu.Unlock()
}

func (m *Mill) notifyChange() {
	m.mu.Lock()
	m.notifyChangeLocked()
	m.mu.Unlock()
}

func (m *Mill) notifyChangeLocked() {
	close(m.changeCh)
	m.changeCh = make(chan struct{})
}

func (m *Mill) currentChangeCh() <-chan struct{} {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.changeCh
}

func (m *Mill) attachSession(sess *millSession) (uint64, bool) {
	// read before taking mu so the db never runs under the mill lock, this
	// also picks up operator cursor resets without a mill restart
	var dbCursor uint64
	haveCursor := false
	if m.db != nil {
		if cur, err := m.db.GetExecutorCursor(sess.nodeID, sess.epoch); err == nil {
			dbCursor, haveCursor = cur, true
		}
	}

	m.mu.Lock()
	defer m.mu.Unlock()

	if old := m.sessions[sess.nodeID]; old != nil {
		if old.live(m.cfg.ReconnectGrace) {
			// a second live session for the same identity is a hijack
			// attempt, reject it
			return 0, false
		}
		if old.graceTimer != nil {
			old.graceTimer.Stop()
		}
		old.close()
		if old.disconnected {
			m.l.Info("executor reconnected", "node", sess.nodeID)
		} else {
			m.l.Warn("replacing silent executor session", "node", sess.nodeID)
		}
	}
	m.sessions[sess.nodeID] = sess
	// wakes commit retries waiting out reconnect grace
	m.notifyChangeLocked()
	key := sess.nodeID + "/" + sess.epoch
	if haveCursor {
		m.nodeSeqno[key] = dbCursor
	}
	return m.nodeSeqno[key], true
}

func (m *Mill) touchSession(sess *millSession) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.sessions[sess.nodeID] != sess || sess.disconnected {
		return false
	}
	sess.lastSeen = time.Now()
	return true
}

func (m *Mill) detachSession(sess *millSession) {
	m.mu.Lock()
	if m.sessions[sess.nodeID] != sess {
		// already replaced by a reconnect
		m.mu.Unlock()
		sess.close()
		return
	}
	sess.disconnected = true
	sess.graceTimer = time.AfterFunc(m.cfg.ReconnectGrace, func() { m.failLeasesAfterGrace(sess) })
	m.mu.Unlock()

	sess.close()
	m.l.Warn("executor session lost; entering reconnect grace", "node", sess.nodeID, "grace", m.cfg.ReconnectGrace)
	m.notifyChange()
}

// an executor whose sessions keep dying on protocol errors is stuck, eg. an
// unrecoverable stream gap, and its reconnects keep resetting grace so its
// leases never fail. after enough strikes quarantine it until an operator
// fixes its stream. clean deaths reset the count
func (m *Mill) noteSessionError(sess *millSession, err error) {
	if !errors.Is(err, errProtocolViolation) {
		m.mu.Lock()
		delete(m.protoStrikes, sess.nodeID)
		m.mu.Unlock()
		return
	}

	m.mu.Lock()
	m.protoStrikes[sess.nodeID]++
	strikes := m.protoStrikes[sess.nodeID]
	m.mu.Unlock()

	m.l.Warn("executor session died on protocol violation", "node", sess.nodeID, "strikes", strikes, "err", err)
	if strikes < m.cfg.QuarantineStrikes || m.db == nil {
		return
	}

	reason := fmt.Sprintf("repeated protocol violations (%d): %v", strikes, err)
	if err := m.db.QuarantineExecutor(sess.nodeID, reason); err != nil {
		m.l.Error("failed to quarantine executor", "node", sess.nodeID, "err", err)
		return
	}
	m.l.Warn("executor quarantined; reset its stream state and unquarantine to readmit", "node", sess.nodeID)
}

func (m *Mill) sessionReady(sess *millSession) {
	for _, lease := range m.cancelledLeasesForNode(sess.nodeID) {
		_, reason := lease.cancelRequested()
		m.sendCancel(sess, lease, reason)
	}
	m.notifyChange()
}

func (m *Mill) hasLiveExecutor(nodeID string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	sess := m.sessions[nodeID]
	return sess != nil && sess.live(m.cfg.ReconnectGrace)
}

func (m *Mill) cancelledLeasesForNode(nodeID string) []*RemoteLease {
	m.mu.Lock()
	var candidates []*RemoteLease
	for _, lease := range m.leases {
		if lease.nodeID == nodeID {
			candidates = append(candidates, lease)
		}
	}
	m.mu.Unlock()

	var leases []*RemoteLease
	for _, lease := range candidates {
		if cancelled, _ := lease.cancelRequested(); cancelled && lease.getState() != leaseDone {
			leases = append(leases, lease)
		}
	}
	return leases
}

// reconnect grace ran out without the executor coming back. fails every
// lease the node held, and reschedules itself if any failure didn't stick
func (m *Mill) failLeasesAfterGrace(sess *millSession) {
	m.mu.Lock()
	if m.sessions[sess.nodeID] != sess || !sess.disconnected {
		// reconnected in the meantime
		m.mu.Unlock()
		return
	}
	var dead []*RemoteLease
	for _, lease := range m.leases {
		if lease.nodeID == sess.nodeID {
			dead = append(dead, lease)
		}
	}
	m.mu.Unlock()

	m.l.Warn("executor declared dead; failing its in-flight jobs", "node", sess.nodeID, "jobs", len(dead))
	deadReason := "executor lost"
	success := true
	for _, lease := range dead {
		switch {
		case lease.orphaned:
			// restored leases have no RunStep waiting, fail them straight
			// into the event stream
			if err := m.finishOrphan(lease, string(models.StatusKindFailed), &deadReason, nil); err != nil {
				m.l.Error("finish orphan after executor loss failed, will retry", "lease", lease.id, "err", err)
				success = false
			}
		default:
			// live leases have a blocked RunStep, keep a pending cancellation
			// as the terminal reason
			status := string(models.StatusKindFailed)
			reason := deadReason
			if cancelled, cancelReason := lease.cancelRequested(); cancelled {
				status = string(models.StatusKindCancelled)
				reason = cancelReason
			}
			if err := m.finishLiveLease(lease, status, reason); err != nil {
				m.l.Error("finish lease after executor loss failed, will retry", "lease", lease.id, "err", err)
				success = false
			}
		}
	}

	m.mu.Lock()
	if !success {
		// something didn't finish cleanly, try again shortly. finished
		// leases skip themselves on the next pass
		sess.graceTimer = time.AfterFunc(5*time.Second, func() { m.failLeasesAfterGrace(sess) })
		m.mu.Unlock()
		return
	}

	// everything failed cleanly. forget the node and wake placement
	if m.sessions[sess.nodeID] == sess {
		delete(m.sessions, sess.nodeID)
	}
	m.mu.Unlock()
	m.notifyChange()
	m.sweepUnclaimedOrphans()
}

func (m *Mill) place(ctx context.Context, engineName string, wid models.WorkflowId, wf *models.Workflow) (engine.WorkflowSlot, error) {
	m.mu.Lock()
	if m.cfg.MaxPending > 0 && m.pending >= m.cfg.MaxPending {
		max := m.cfg.MaxPending
		cur := m.pending
		m.mu.Unlock()
		return nil, fmt.Errorf("%w: mill has %d pending jobs (max %d)", engine.ErrNoWorkflowSlots, cur, max)
	}
	m.pending++
	m.mu.Unlock()
	defer func() {
		m.mu.Lock()
		m.pending--
		m.mu.Unlock()
	}()

	for {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		// grab the channel before bidding. a change mid-bid closes it, so
		// the wait below re-bids right away
		ch := m.currentChangeCh()

		lease, err := m.bid(ctx, engineName, wid, wf)
		if err != nil {
			return nil, err
		}
		if lease != nil {
			lease.wid = wid
			if err := m.persistLease(lease, leaseRowReserved); err != nil {
				m.releaseRemote(lease)
				m.mu.Lock()
				delete(m.reservations, lease.id)
				m.mu.Unlock()
				return nil, fmt.Errorf("persist reserved mill lease: %w", err)
			}
			m.mu.Lock()
			delete(m.reservations, lease.id)
			m.leases[lease.id] = lease
			if st, ok := wf.Data.(*millWorkflowState); ok && st != nil {
				st.Lease = lease
			}
			m.mu.Unlock()
			return &millSlot{fleet: m, lease: lease}, nil
		}

		// no executor available. wait for a change or ctx
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-ch:
		}
	}
}

func (m *Mill) bid(ctx context.Context, engineName string, wid models.WorkflowId, wf *models.Workflow) (*RemoteLease, error) {
	rawPipeline, rawWorkflow, err := marshalJob(wf)
	if err != nil {
		return nil, err
	}

	requiredLabels := requiredLabels(wf)
	candidates := m.rankCandidates(engineName, requiredLabels)
	if len(candidates) == 0 {
		return nil, nil
	}

	type bidResult struct {
		sess         *millSession
		lease        *RemoteLease
		rank         int
		incompatible bool
		reason       string
	}
	limit := m.cfg.TopK
	if limit <= 0 {
		limit = len(candidates)
	}
	if len(candidates) > limit {
		candidates = candidates[:limit]
	}
	results := make(chan bidResult, limit)
	ask := func(rank int, sess *millSession) {
		bidCtx, cancel := context.WithTimeout(ctx, m.cfg.BidTimeout)
		defer cancel()
		leaseID := m.nextLeaseID()
		lease := newLease(leaseID, sess.nodeID, sess.epoch, engineName)
		m.mu.Lock()
		m.reservations[leaseID] = lease
		m.mu.Unlock()
		msg := &millproto.Message{ReserveSeat: &millv1.ReserveSeat{
			LeaseId:         leaseID,
			TargetEngine:    engineName,
			RawPipelineJson: rawPipeline,
			RawWorkflowJson: rawWorkflow,
			Knot:            wid.Knot,
			Rkey:            wid.Rkey,
			TtlSeconds:      uint32(m.cfg.ReconnectGrace / time.Second),
		}}
		resp, err := sess.request(bidCtx, leaseID, msg)
		if err != nil {
			m.dropReservation(leaseID)
			results <- bidResult{}
			return
		}
		rr := resp.GetReserveResult()
		if rr == nil {
			m.dropReservation(leaseID)
			results <- bidResult{}
			return
		}
		if !rr.GetAccepted() {
			m.dropReservation(leaseID)
			if rr.GetRejectClass() == millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE {
				results <- bidResult{sess: sess, rank: rank, incompatible: true, reason: rr.GetRejectReason()}
				return
			}
			results <- bidResult{}
			return
		}
		results <- bidResult{sess: sess, lease: lease, rank: rank}
	}
	next := 0
	inFlight := 0
	for next < len(candidates) && inFlight < limit {
		inFlight++
		go ask(next, candidates[next])
		next++
	}

	var winner *bidResult
	var losers []*RemoteLease
	var incompatible []string
	// any soft reject (transient or timeout) means the fleet was just
	// busy, so an all-incompatible outcome isn't a hard placement error
	softReject := false
	for inFlight > 0 {
		r := <-results
		inFlight--
		// incompatible rejects get reported to the user, any other failure
		// just means the fleet is busy
		if r.incompatible {
			if r.reason != "" {
				incompatible = append(incompatible, r.reason)
			}
		} else if r.lease == nil {
			softReject = true
		}
		// a failed bid means ask the next candidate, unless someone won
		if r.lease == nil {
			for winner == nil && next < len(candidates) && inFlight < limit {
				inFlight++
				go ask(next, candidates[next])
				next++
			}
			continue
		}
		// if this bid is worse than the winner it goes to the losers pile
		if winner != nil && r.rank >= winner.rank {
			losers = append(losers, r.lease)
			continue
		}
		// otherwise it's the new best and the old winner joins the losers
		if winner != nil {
			losers = append(losers, winner.lease)
		}
		winner = &r
	}

	// let the losers go so they free their seats right away
	for _, l := range losers {
		m.dropReservation(l.id)
		m.releaseRemote(l)
	}

	if winner == nil {
		if len(incompatible) > 0 && !softReject {
			return nil, fmt.Errorf("no compatible executor for %s: %s", engineName, strings.Join(incompatible, "; "))
		}
		return nil, nil
	}
	return winner.lease, nil
}

// ranks nodes that are least busy first. if a resource is used a lot
// then that node will lose to one that is more even across the board.
func (m *Mill) rankCandidates(engineName string, requiredLabels []string) []*millSession {
	m.mu.Lock()
	defer m.mu.Unlock()

	type ranked struct {
		sess  *millSession
		worst float64
		sum   float64
	}
	var rs []ranked
	for _, s := range m.sessions {
		// only live, reporting sessions can take work
		if s.disconnected {
			continue
		}
		if s.snapshot == nil {
			continue
		}
		// the engine has to exist and have room right now
		ea, ok := s.snapshot.GetEngines()[engineName]
		if !ok || !ea.GetAvailable() {
			continue
		}
		// and satisfy the wf's label requirements
		if !hasLabels(s.labels, requiredLabels) {
			continue
		}
		worst, sum := loadScore(ea.GetLoad())
		rs = append(rs, ranked{sess: s, worst: worst, sum: sum})
	}
	slices.SortStableFunc(rs, func(a, b ranked) int {
		if a.worst < b.worst {
			return -1
		}
		if a.worst > b.worst {
			return 1
		}
		if a.sum < b.sum {
			return -1
		}
		if a.sum > b.sum {
			return 1
		}
		return 0
	})

	out := make([]*millSession, len(rs))
	for i := range rs {
		out[i] = rs[i].sess
	}
	return out
}

func loadScore(load map[string]float64) (worst, sum float64) {
	for _, v := range load {
		if v > worst {
			worst = v
		}
		sum += v
	}
	return worst, sum
}

func requiredLabels(wf *models.Workflow) []string {
	st, ok := wf.Data.(*millWorkflowState)
	if !ok || st == nil {
		return nil
	}
	return st.RawWorkflow.RunsOn
}

func hasLabels(labels []string, required []string) bool {
	for _, want := range required {
		if !slices.Contains(labels, want) {
			return false
		}
	}
	return true
}

func (m *Mill) commitAndWait(ctx context.Context, wf *models.Workflow, unlocked []secrets.UnlockedSecret) error {
	st, ok := wf.Data.(*millWorkflowState)
	if !ok || st == nil || st.Lease == nil {
		return fmt.Errorf("mill workflow state missing lease")
	}
	lease := st.Lease

	pbSecrets := make([]*millv1.Secret, len(unlocked))
	for i, s := range unlocked {
		pbSecrets[i] = &millv1.Secret{Key: s.Key, Value: s.Value}
	}

	commit := &millproto.Message{CommitLease: &millv1.CommitLease{
		LeaseId: lease.id,
		Secrets: pbSecrets,
	}}

	// commit retries ride reconnects, a reservation outlives one
	// disconnect. lost session or slow executor just means wait and retry,
	// only job timeout or a dead lease stops the loop
	for {
		if res, ok := pollTerminal(lease); ok {
			return terminalError(res.Status)
		}
		if !lease.markCommitting() {
			return engine.ErrWorkflowFailed
		}

		sess := m.sessionForNode(lease.nodeID)
		if sess == nil {
			if done, err := m.waitCommitRetry(ctx, lease); done || err != nil {
				return err
			}
			continue
		}

		reqCtx, cancel := context.WithTimeout(ctx, m.cfg.BidTimeout)
		resp, err := sess.request(reqCtx, lease.id, commit)
		cancel()
		if err != nil {
			switch {
			case errors.Is(err, errSessionClosed):
				// session died mid-request. wait out the grace, then retry on
				// the new one
				if done, err := m.waitCommitRetry(ctx, lease); done || err != nil {
					return err
				}
				continue
			case errors.Is(err, context.DeadlineExceeded) && ctx.Err() == nil:
				// executor didn't answer in time, but its seat is still held so
				// retrying is safe
				continue
			case errors.Is(err, context.DeadlineExceeded):
				// the job ctx itself ran out, a real timeout
				return engine.ErrTimedOut
			case errors.Is(err, context.Canceled):
				return err
			default:
				m.l.Warn("commit lease send failed; waiting for reconnect", "lease", lease.id, "node", lease.nodeID, "err", err)
				if done, err := m.waitCommitRetry(ctx, lease); done || err != nil {
					return err
				}
				continue
			}
		}
		if resp.GetCommitted() == nil {
			return engine.ErrWorkflowFailed
		}
		lease.markRunning()
		if err := m.persistLease(lease, leaseRowRunning); err != nil {
			m.l.Error("persist running mill lease", "lease", lease.id, "err", err)
		}
		if cancelled, reason := lease.cancelRequested(); cancelled {
			m.sendCancel(sess, lease, reason)
		}
		break
	}

	select {
	case res := <-lease.terminal:
		return terminalError(res.Status)
	case <-ctx.Done():
		if ctx.Err() == context.DeadlineExceeded {
			return engine.ErrTimedOut
		}
		return ctx.Err()
	}
}

func (m *Mill) waitCommitRetry(ctx context.Context, lease *RemoteLease) (bool, error) {
	// grab the channel before checking for a live session again
	// a reconnect will still close the channel if it happens in between
	ch := m.currentChangeCh()
	if m.sessionForNode(lease.nodeID) != nil {
		return false, nil
	}
	select {
	case res := <-lease.terminal:
		return true, terminalError(res.Status)
	case <-ctx.Done():
		if ctx.Err() == context.DeadlineExceeded {
			return true, engine.ErrTimedOut
		}
		return true, ctx.Err()
	case <-ch:
		return false, nil
	}
}

func terminalError(status millv1.TerminalStatus) error {
	switch status {
	case millv1.TerminalStatus_SUCCESS:
		return nil
	case millv1.TerminalStatus_TIMEOUT:
		return engine.ErrTimedOut
	case millv1.TerminalStatus_CANCELLED:
		return engine.ErrWorkflowCanceled
	default:
		return engine.ErrWorkflowFailed
	}
}

func pollTerminal(lease *RemoteLease) (*millv1.AttemptResult, bool) {
	select {
	case res := <-lease.terminal:
		return res, true
	default:
		return nil, false
	}
}

func (m *Mill) destroy(wid models.WorkflowId) {
	m.mu.Lock()
	var lease *RemoteLease
	for _, l := range m.leases {
		if l.wid == wid {
			lease = l
			break
		}
	}
	m.mu.Unlock()
	if lease == nil {
		return
	}
	reason := "workflow destroyed"
	switch lease.requestCancel(reason) {
	case cancelLocal:
		if sess := m.sessionForNode(lease.nodeID); sess != nil {
			_ = sess.send(&millproto.Message{ReleaseLease: &millv1.ReleaseLease{LeaseId: lease.id}})
		}
		lease.deliverCancelled(reason)
	case cancelRemote:
		if sess := m.sessionForNode(lease.nodeID); sess != nil {
			m.sendCancel(sess, lease, reason)
		}
	}
}

func (m *Mill) releaseSlot(s *millSlot) {
	lease := s.lease
	state, cancelled := lease.releaseState()
	if state == leaseReserved {
		m.releaseRemote(lease)
	} else if cancelled && state != leaseDone {
		return
	}
	if err := m.cleanupLease(lease); err != nil {
		m.l.Error("releaseSlot cleanupLease failed", "lease", lease.id, "err", err)
	}
}
func (m *Mill) cleanupLease(lease *RemoteLease) error {
	lease.finishMu.Lock()
	defer lease.finishMu.Unlock()
	return m.cleanupLeaseLocked(lease)
}

func (m *Mill) cleanupLeaseLocked(lease *RemoteLease) error {
	if lease.cleanedUp {
		return nil
	}
	if m.db != nil {
		if err := m.db.DeleteMillLease(lease.id); err != nil {
			m.scheduleCleanupLocked(lease)
			return err
		}
	}
	m.mu.Lock()
	delete(m.leases, lease.id)
	m.mu.Unlock()
	lease.cleanedUp = true
	m.notifyChange()
	return nil
}

func (m *Mill) scheduleCleanupLocked(lease *RemoteLease) {
	if lease.cleanedUp || lease.cleanupRetry {
		return
	}
	lease.cleanupRetry = true
	time.AfterFunc(5*time.Second, func() {
		lease.finishMu.Lock()
		lease.cleanupRetry = false
		err := m.cleanupLeaseLocked(lease)
		lease.finishMu.Unlock()
		if err != nil {
			m.l.Error("retry lease cleanup failed", "lease", lease.id, "err", err)
		}
	})
}

func (m *Mill) releaseRemote(lease *RemoteLease) {
	lease.setState(leaseDone)
	if sess := m.sessionForNode(lease.nodeID); sess != nil {
		_ = sess.send(&millproto.Message{ReleaseLease: &millv1.ReleaseLease{LeaseId: lease.id}})
	}
}

func (m *Mill) sendCancel(sess *millSession, lease *RemoteLease, reason string) {
	if err := sess.send(&millproto.Message{CancelAttempt: &millv1.CancelAttempt{
		LeaseId: lease.id,
		Reason:  reason,
	}}); err != nil {
		return
	}
	time.AfterFunc(m.cfg.CancelTimeout, func() { m.checkCancelDeadline(lease) })
}

func (m *Mill) sessionForNode(nodeID string) *millSession {
	m.mu.Lock()
	defer m.mu.Unlock()
	sess := m.sessions[nodeID]
	if sess == nil || sess.disconnected {
		return nil
	}
	return sess
}

func (m *Mill) onSnapshot(sess *millSession, snap *millv1.NodeSnapshot) error {
	m.mu.Lock()
	if sess.snapshot != nil && snap.Seqno <= sess.snapshot.Seqno {
		m.mu.Unlock()
		return protoErrf("snapshot seqno regression. Got %d, last seen %d", snap.Seqno, sess.snapshot.Seqno)
	}
	sess.snapshot = snap
	m.mu.Unlock()

	if err := m.reconcileLeases(sess, snap.GetActiveLeaseIds()); err != nil {
		return err
	}
	m.mu.Lock()
	for _, id := range snap.GetActiveLeaseIds() {
		if lease := m.leases[id]; lease != nil && lease.nodeID == sess.nodeID && lease.epoch == sess.epoch {
			lease.claimed = true
		}
	}
	m.mu.Unlock()
	m.notifyChange()
	return nil
}

func (m *Mill) onEventBatch(sess *millSession, batch *millv1.EventBatch) error {
	if batch == nil {
		return nil
	}

	if batch.Epoch != sess.epoch {
		return protoErrf("batch epoch %q does not match session %q", batch.Epoch, sess.epoch)
	}

	m.mu.Lock()
	currentKey := sess.nodeID + "/" + sess.epoch
	current := m.nodeSeqno[currentKey]
	m.mu.Unlock()

	expected := current + 1
	var newEntries []*millv1.Event
	for _, entry := range batch.Events {
		// reconnect replays old seqnos, drop those
		if entry.Seqno <= current {
			continue
		}
		// gap means executor lost rows. so dont apply a partial batch
		if entry.Seqno != expected {
			return protoErrf("gap in stream seqnos. Expected %d, got %d", expected, entry.Seqno)
		}
		newEntries = append(newEntries, entry)
		expected++
	}

	// all replays, still ack so the executor can trim its outbox
	if len(newEntries) == 0 {
		return m.sendAck(sess, current)
	}

	type pendingTerminal struct {
		lease *RemoteLease
		ar    *millv1.AttemptResult
	}
	var pendingTerminals []pendingTerminal
	var artifactLeases []*RemoteLease
	finishedInBatch := make(map[string]struct{})

	var highestSeqno uint64 = current

	applyFunc := func(tx *db.EventBatchTx) error {
		for _, entry := range newEntries {
			m.mu.Lock()
			lease := m.leases[entry.LeaseId]
			m.mu.Unlock()

			// events for leases this node doesn't own are skipped but still
			// count as processed
			if lease == nil || lease.nodeID != sess.nodeID {
				highestSeqno = entry.Seqno
				continue
			}

			// events arriving for a different epoch are invalid (different session)
			if lease.epoch != "" && lease.epoch != sess.epoch {
				return protoErrf("lease %q epoch %q does not match session %q", lease.id, lease.epoch, sess.epoch)
			}

			lease.mu.Lock()
			state := lease.state
			lease.mu.Unlock()

			// done leases can replay terminals on reconnect, skip them
			if state == leaseDone {
				highestSeqno = entry.Seqno
				continue
			}

			if _, finished := finishedInBatch[lease.id]; finished {
				return protoErrf("stream entry follows terminal for lease %q", lease.id)
			}

			switch {
			case entry.GetStatusEvent() != nil:
				ev := entry.GetStatusEvent()
				statusStr := string(models.StatusKindRunning)
				var errMsg *string
				if e := ev.GetError(); e != "" {
					errMsg = &e
				}
				var exitCode *int64
				if c := ev.GetExitCode(); c != 0 {
					exitCode = &c
				}
				pipelineAtUri := string(lease.wid.PipelineId.AtUri())
				if tx != nil {
					if err := tx.InsertStatusEvent(pipelineAtUri, lease.wid.Name, statusStr, errMsg, exitCode); err != nil {
						return err
					}
				}

			case entry.GetAttemptResult() != nil:
				ar := entry.GetAttemptResult()
				statusStr := "success"
				switch ar.Status {
				case millv1.TerminalStatus_SUCCESS:
					statusStr = "success"
				case millv1.TerminalStatus_FAILED:
					statusStr = "failed"
				case millv1.TerminalStatus_TIMEOUT:
					statusStr = "timeout"
				case millv1.TerminalStatus_CANCELLED:
					statusStr = "cancelled"
				default:
					return protoErrf("unsupported terminal status %v", ar.Status)
				}
				var errMsg *string
				if e := ar.GetError(); e != "" {
					errMsg = &e
				}
				var exitCode *int64
				if c := ar.GetExitCode(); c != 0 {
					exitCode = &c
				}
				finishedInBatch[lease.id] = struct{}{}
				pipelineAtUri := string(lease.wid.PipelineId.AtUri())
				if tx != nil {
					if err := tx.InsertStatusEvent(pipelineAtUri, lease.wid.Name, statusStr, errMsg, exitCode); err != nil {
						return err
					}
					if err := tx.DeleteLease(lease.id); err != nil {
						return err
					}
					if a := ar.GetLogArtifact(); a != nil {
						if a.GetRef() == "" {
							return protoErrf("empty log artifact ref")
						}
						if !strings.HasPrefix(a.GetHash(), "sha256:") {
							return protoErrf("invalid log artifact hash %q", a.GetHash())
						}
						if tx != nil {
							if err := tx.InsertArtifactRef(lease.id, lease.wid.Name, a.GetRef(), a.GetHash()); err != nil {
								return err
							}
						}
						artifactLeases = append(artifactLeases, lease)
					}
				}
				pendingTerminals = append(pendingTerminals, pendingTerminal{
					lease: lease,
					ar:    ar,
				})
			}

			highestSeqno = entry.Seqno
		}

		if tx != nil {
			return tx.AdvanceCursor(sess.nodeID, sess.epoch, highestSeqno)
		}
		return nil
	}

	var err error
	if m.db != nil {
		err = m.db.ApplyEventBatch(m.n, applyFunc)
	} else {
		err = applyFunc(nil)
	}

	if err != nil {
		return err
	}

	if m.cfg.LogDir != "" {
		for _, lease := range artifactLeases {
			path := models.LogFilePath(m.cfg.LogDir, lease.wid)
			if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
				m.l.Warn("failed to remove live log file after artifact recorded", "path", path, "err", err)
			}
		}
	}

	m.mu.Lock()
	if highestSeqno > m.nodeSeqno[currentKey] {
		m.nodeSeqno[currentKey] = highestSeqno
	}
	m.mu.Unlock()

	for _, pt := range pendingTerminals {
		// orphans have no waiting RunStep, just mark and clean up
		if pt.lease.orphaned {
			pt.lease.markDone()
			_ = m.cleanupLease(pt.lease)
			continue
		}
		pt.lease.deliverTerminal(pt.ar)
		if pt.lease.cleanupReady() {
			_ = m.cleanupLease(pt.lease)
		}
	}

	return m.sendAck(sess, highestSeqno)
}

func (m *Mill) sendAck(sess *millSession, seqno uint64) error {
	msg := &millproto.Message{Ack: &millv1.Ack{
		Epoch:     sess.epoch,
		UpToSeqno: seqno,
	}}
	if err := sess.send(msg); err != nil {
		return fmt.Errorf("send ack message: %w", err)
	}
	return nil
}
func (m *Mill) onLiveLog(sess *millSession, ll *millv1.LiveLog) error {
	if ll == nil || ll.GetLeaseId() == "" {
		return nil
	}
	m.mu.Lock()
	lease := m.leases[ll.GetLeaseId()]
	m.mu.Unlock()
	if lease == nil || lease.nodeID != sess.nodeID {
		return nil
	}
	raw := ll.GetRawJson()
	if m.cfg.LogDir == "" || len(raw) == 0 {
		if m.n != nil {
			m.n.NotifyAll()
		}
		return nil
	}
	lease.mu.Lock()
	isDone := (lease.state == leaseDone)
	lease.mu.Unlock()
	if isDone {
		if m.n != nil {
			m.n.NotifyAll()
		}
		return nil
	}

	logPath := models.LogFilePath(m.cfg.LogDir, lease.wid)
	if err := os.MkdirAll(filepath.Dir(logPath), 0755); err != nil {
		m.l.Warn("failed to create log dir", "path", filepath.Dir(logPath), "err", err)
		if m.n != nil {
			m.n.NotifyAll()
		}
		return nil
	}
	f, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		m.l.Warn("failed to open log file", "path", logPath, "err", err)
		if m.n != nil {
			m.n.NotifyAll()
		}
		return nil
	}
	if _, err := f.Write(raw); err != nil {
		m.l.Warn("failed to write log file", "path", logPath, "err", err)
	}
	_ = f.Close()

	if m.n != nil {
		m.n.NotifyAll()
	}
	return nil
}

func (m *Mill) onCancelAck(sess *millSession, ca *millv1.CancelAck) {
	m.mu.Lock()
	lease := m.leases[ca.GetLeaseId()]
	m.mu.Unlock()
	if lease == nil || lease.nodeID != sess.nodeID {
		return
	}
	lease.mu.Lock()
	lease.cancelAcked = true
	lease.mu.Unlock()
}

func (m *Mill) checkCancelDeadline(lease *RemoteLease) {
	lease.mu.Lock()
	isDone := (lease.state == leaseDone)
	isAcked := lease.cancelAcked
	lease.mu.Unlock()

	if isDone && isAcked {
		return
	}

	m.l.Warn("node failed to comply with cancel request within deadline, quarantining", "node", lease.nodeID, "lease", lease.id, "done", isDone, "acked", isAcked)

	reason := fmt.Sprintf("cancel noncompliance for lease %s (done: %t, acked: %t)", lease.id, isDone, isAcked)
	if m.db != nil {
		if err := m.db.QuarantineExecutor(lease.nodeID, reason); err != nil {
			m.l.Error("failed to quarantine executor", "node", lease.nodeID, "err", err)
		}
	}

	m.mu.Lock()
	sess := m.sessions[lease.nodeID]
	m.mu.Unlock()
	if sess != nil {
		sess.close()
	}
}

func marshalJob(wf *models.Workflow) (pipeline string, workflow string, err error) {
	st, ok := wf.Data.(*millWorkflowState)
	if !ok || st == nil {
		return "", "", fmt.Errorf("mill workflow state missing")
	}
	p, err := json.Marshal(st.RawPipeline)
	if err != nil {
		return "", "", fmt.Errorf("marshal pipeline: %w", err)
	}
	w, err := json.Marshal(st.RawWorkflow)
	if err != nil {
		return "", "", fmt.Errorf("marshal workflow: %w", err)
	}
	return string(p), string(w), nil
}
