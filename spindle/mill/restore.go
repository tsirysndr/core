package mill

import (
	"fmt"
	"tangled.org/core/spindle/db"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
	"time"
)

const (
	leaseRowReserved = "reserved"
	leaseRowRunning  = "running"
)

func (m *Mill) persistLease(lease *RemoteLease, state string) error {
	if m.db == nil {
		return nil
	}
	return m.db.SaveMillLease(db.MillLease{
		LeaseID:  lease.id,
		NodeID:   lease.nodeID,
		Epoch:    lease.epoch,
		Engine:   lease.engine,
		Knot:     lease.wid.Knot,
		Rkey:     lease.wid.Rkey,
		Workflow: lease.wid.Name,
		State:    state,
	})
}

func (m *Mill) RestoreState() error {
	if m.db == nil {
		return nil
	}
	cursors, err := m.db.ListExecutorCursors()
	if err != nil {
		return err
	}
	rows, err := m.db.ListMillLeases()
	if err != nil {
		return err
	}

	m.mu.Lock()
	for _, c := range cursors {
		m.nodeSeqno[c.NodeID+"/"+c.Epoch] = c.AckedSeqno
	}
	for _, r := range rows {
		lease := newLease(r.LeaseID, r.NodeID, r.Epoch, r.Engine)
		lease.wid = models.WorkflowId{
			PipelineId: models.PipelineId{Knot: r.Knot, Rkey: r.Rkey},
			Name:       r.Workflow,
		}
		// restored leases start as orphans, an executor must reclaim it via
		// its first snapshot, or the sweep will fail it
		lease.orphaned = true
		lease.claimed = false
		if r.State == leaseRowRunning {
			lease.state = leaseRunning
		}
		m.leases[r.LeaseID] = lease
	}
	restored := len(rows)
	m.mu.Unlock()

	if restored > 0 {
		m.l.Info("restored mill leases from previous run", "leases", restored, "cursors", len(cursors))
		// executors get a grace window to reconnect and claim their leases
		time.AfterFunc(m.cfg.ReconnectGrace, m.sweepUnclaimedOrphans)
	}
	return nil
}
func (m *Mill) sweepUnclaimedOrphans() {
	m.mu.Lock()
	var unclaimed []*RemoteLease
	for _, lease := range m.leases {
		if !lease.orphaned {
			continue
		}
		if lease.claimed {
			continue
		}
		if lease.getState() == leaseDone {
			continue
		}
		if sess := m.sessions[lease.nodeID]; sess == nil || sess.disconnected {
			unclaimed = append(unclaimed, lease)
		}
	}
	m.mu.Unlock()

	retry := false
	reason := "executor did not reconnect after mill restart"
	for _, lease := range unclaimed {
		m.l.Warn("failing unclaimed restored lease", "lease", lease.id, "node", lease.nodeID)
		if err := m.finishOrphan(lease, string(models.StatusKindFailed), &reason, nil); err != nil {
			m.l.Error("finish unclaimed restored lease", "lease", lease.id, "err", err)
			retry = true
		}
	}
	if retry {
		time.AfterFunc(5*time.Second, m.sweepUnclaimedOrphans)
	}
}

func (m *Mill) reconcileLeases(sess *millSession, activeLeaseIDs []string) error {
	active := make(map[string]struct{}, len(activeLeaseIDs))
	for _, id := range activeLeaseIDs {
		active[id] = struct{}{}
	}

	known := make(map[string]struct{})
	m.mu.Lock()
	var gone []*RemoteLease
	for _, lease := range m.leases {
		if lease.nodeID == sess.nodeID {
			known[lease.id] = struct{}{}
			if lease.epoch != sess.epoch {
				gone = append(gone, lease)
			} else if _, ok := active[lease.id]; !ok {
				gone = append(gone, lease)
			}
		}
	}
	for _, lease := range m.reservations {
		if lease.nodeID == sess.nodeID && lease.epoch == sess.epoch {
			known[lease.id] = struct{}{}
		}
	}
	m.mu.Unlock()
	var unknown []string
	for id := range active {
		if _, ok := known[id]; !ok {
			unknown = append(unknown, id)
		}
	}

	for _, lease := range gone {
		status := string(models.StatusKindFailed)
		reason := "executor no longer holds lease"
		if cancelled, cancelReason := lease.cancelRequested(); cancelled {
			status = string(models.StatusKindCancelled)
			reason = cancelReason
		}
		m.l.Warn("finishing reconciled lease", "lease", lease.id, "node", sess.nodeID, "leaseInc", lease.epoch, "sessInc", sess.epoch)
		if lease.orphaned {
			if err := m.finishOrphan(lease, status, &reason, nil); err != nil {
				return err
			}
		} else if err := m.finishLiveLease(lease, status, reason); err != nil {
			return err
		}
	}
	for _, id := range unknown {
		if err := sess.send(&millproto.Message{CancelAttempt: &millv1.CancelAttempt{LeaseId: id, Reason: "lease is not owned by this mill"}}); err != nil {
			return fmt.Errorf("cancel unknown executor lease %q: %w", id, err)
		}
	}
	return nil
}

func (m *Mill) completeLeaseRow(lease *RemoteLease, status string, errMsg *string, exitCode *int64) error {
	if m.db == nil {
		return nil
	}
	return m.db.CompleteMillLease(
		lease.id,
		string(lease.wid.PipelineId.AtUri()),
		lease.wid.Name,
		status,
		errMsg,
		exitCode,
		m.n,
	)
}

func (m *Mill) finishLiveLease(lease *RemoteLease, status, reason string) error {
	lease.finishMu.Lock()
	defer lease.finishMu.Unlock()
	if lease.getState() == leaseDone {
		return nil
	}
	if err := m.completeLeaseRow(lease, status, &reason, nil); err != nil {
		return err
	}
	lease.deliverTerminal(&millv1.AttemptResult{
		Status: mapTerminalStatusString(status),
		Error:  reason,
	})
	return m.cleanupLeaseLocked(lease)
}

func (m *Mill) finishOrphan(lease *RemoteLease, status string, errMsg *string, exitCode *int64) error {
	lease.finishMu.Lock()
	defer lease.finishMu.Unlock()
	if lease.getState() == leaseDone {
		return nil
	}
	if err := m.completeLeaseRow(lease, status, errMsg, exitCode); err != nil {
		return err
	}
	lease.markDone()
	return m.cleanupLeaseLocked(lease)
}

func mapTerminalStatusString(s string) millv1.TerminalStatus {
	switch s {
	case "success":
		return millv1.TerminalStatus_SUCCESS
	case "failed":
		return millv1.TerminalStatus_FAILED
	case "timeout":
		return millv1.TerminalStatus_TIMEOUT
	case "cancelled":
		return millv1.TerminalStatus_CANCELLED
	default:
		return millv1.TerminalStatus_TERMINAL_STATUS_UNSPECIFIED
	}
}
