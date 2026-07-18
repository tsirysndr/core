package executor

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"google.golang.org/protobuf/proto"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/db"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
)

const (
	maxBatchBytes  = 4 * 1024 * 1024
	maxBatchEvents = 128
)

func generateEpoch() string {
	var b [8]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}

func (e *Executor) initOutbox() error {
	e.eventMu.Lock()
	defer e.eventMu.Unlock()

	epoch, _, err := e.db.GetOutboxState()
	if err != nil {
		return err
	}
	if epoch != "" {
		e.epoch = epoch
	} else {
		e.epoch = generateEpoch()
		if err := e.db.SetOutboxEpoch(e.epoch); err != nil {
			return fmt.Errorf("set outbox epoch: %w", err)
		}
	}

	rows, err := e.db.ListOutboxRows()
	if err != nil {
		return fmt.Errorf("list outbox rows: %w", err)
	}

	for _, r := range rows {
		e.outboxBytes += r.ByteSize
	}
	_ = e.recoverPendingArtifacts()
	return nil
}

func (e *Executor) appendAndSend(leaseID string, payload any, control bool) error {
	entry := &millv1.Event{LeaseId: leaseID}
	switch payload := payload.(type) {
	case *millv1.Event_StatusEvent:
		entry.Payload = payload
	case *millv1.Event_AttemptResult:
		entry.Payload = payload
	default:
		return fmt.Errorf("unsupported stream payload %T", payload)
	}
	isTerminal := false
	if _, ok := payload.(*millv1.Event_AttemptResult); ok {
		isTerminal = true
	}
	entry.Seqno = ^uint64(0)
	wireSize := proto.Size(&millproto.Message{EventBatch: &millv1.EventBatch{
		Epoch:  e.epoch,
		Events: []*millv1.Event{entry},
	}})
	entry.Seqno = 0
	if wireSize > maxBatchBytes {
		return fmt.Errorf("control stream entry exceeds wire limit: %d > %d", wireSize, maxBatchBytes)
	}

	encoded, err := proto.Marshal(entry)
	if err != nil {
		return fmt.Errorf("marshal stream entry: %w", err)
	}

	e.eventMu.Lock()
	if control && !isTerminal && e.maxOutboxBytes > 0 && e.outboxBytes+int64(len(encoded)) > e.maxOutboxBytes {
		e.l.Warn("outbox reserve exhausted; dropping nonterminal status", "cap", e.maxOutboxBytes)
		e.eventMu.Unlock()
		return nil
	}
	if _, err := e.db.AppendOutboxRow(encoded, control); err != nil {
		e.eventMu.Unlock()
		return fmt.Errorf("append outbox row: %w", err)
	}
	e.outboxBytes += int64(len(encoded))
	e.eventMu.Unlock()

	e.sendPending()
	return nil
}

func (e *Executor) appendStatus(leaseID string, st *tangled.PipelineStatus) error {
	if st.Status != string(models.StatusKindRunning) {
		return fmt.Errorf("unsupported nonterminal status %q", st.Status)
	}
	exit, errStr := parseStatusExitAndError(st)
	payload := &millv1.Event_StatusEvent{StatusEvent: &millv1.StatusEvent{
		Status:   millv1.NonterminalStatus_RUNNING,
		Error:    errStr,
		ExitCode: exit,
	}}
	return e.appendAndSend(leaseID, payload, true)
}

func (e *Executor) appendTerminal(leaseID, status string, st *tangled.PipelineStatus) error {
	return e.appendTerminalWithArtifact(leaseID, status, st, "", "")
}

func (e *Executor) appendTerminalWithArtifact(leaseID, status string, st *tangled.PipelineStatus, ref, hash string) error {
	var terminalStatus millv1.TerminalStatus
	switch status {
	case string(models.StatusKindSuccess):
		terminalStatus = millv1.TerminalStatus_SUCCESS
	case string(models.StatusKindFailed):
		terminalStatus = millv1.TerminalStatus_FAILED
	case string(models.StatusKindTimeout):
		terminalStatus = millv1.TerminalStatus_TIMEOUT
	case string(models.StatusKindCancelled):
		terminalStatus = millv1.TerminalStatus_CANCELLED
	default:
		return fmt.Errorf("unsupported terminal status %q", status)
	}

	exit, errStr := parseStatusExitAndError(st)
	var logArtifact *millv1.LogArtifact
	if ref != "" {
		logArtifact = &millv1.LogArtifact{
			Ref:  ref,
			Hash: hash,
		}
	}
	payload := &millv1.Event_AttemptResult{AttemptResult: &millv1.AttemptResult{
		Status:      terminalStatus,
		Error:       errStr,
		ExitCode:    exit,
		LogArtifact: logArtifact,
	}}
	return e.appendAndSend(leaseID, payload, true)
}

func (e *Executor) recoverPendingArtifacts() error {
	if e.db == nil {
		return nil
	}
	pending, err := e.db.ListPendingArtifacts()
	if err != nil || len(pending) == 0 {
		return err
	}
	for _, p := range pending {
		if p.Ref != "" {
			if e.writer == nil {
				continue
			}
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
			var logDir string
			if e.cfg != nil {
				logDir = e.cfg.Server.LogDir
			}
			logPath := models.LogFilePath(logDir, models.WorkflowId{Name: p.Workflow})
			f, openErr := os.Open(logPath)
			if openErr != nil {
				cancel()
				continue
			}
			uploadErr := e.writer.Put(ctx, p.Ref, f)
			_ = f.Close()
			cancel()
			if uploadErr != nil {
				continue
			}
		}

		st := &tangled.PipelineStatus{
			Status:   p.Status,
			Error:    &p.Error,
			ExitCode: &p.ExitCode,
		}
		if err := e.appendTerminalWithArtifact(p.LeaseID, p.Status, st, p.Ref, p.Hash); err == nil {
			_ = e.db.RemovePendingArtifact(p.LeaseID)
		}
	}
	return nil
}

func truncateEventString(value string) string {
	if len(value) > 65536 {
		return value[:65536]
	}
	return value
}

func (e *Executor) sendPending() {
	e.connMu.Lock()
	enc := e.enc
	e.connMu.Unlock()

	if enc == nil {
		return
	}

	e.flushMu.Lock()
	defer e.flushMu.Unlock()

	if err := e.sendPendingLocked(enc); err != nil {
		e.l.Error("send pending events failed", "err", err)
	}
}

func (e *Executor) sendPendingLocked(enc messageEncoder) error {
	for {
		rows, err := e.db.ListOutboxRowsAfter(e.sentSeqno, maxBatchEvents)
		if err != nil {
			return fmt.Errorf("list outbox rows after %d: %w", e.sentSeqno, err)
		}
		if len(rows) == 0 {
			return nil
		}
		if err := e.sendRows(enc, rows); err != nil {
			return err
		}
	}
}

func (e *Executor) sendRows(enc messageEncoder, rows []db.OutboxRow) error {
	events := make([]*millv1.Event, 0, len(rows))
	var lastSeqno uint64

	for _, r := range rows {
		ev, err := decodeEvent(r.Payload, r.Seqno)
		if err != nil {
			return fmt.Errorf("decode event at seqno %d: %w", r.Seqno, err)
		}
		events = append(events, ev)
		lastSeqno = r.Seqno
	}

	batch := &millv1.EventBatch{
		Epoch:  e.epoch,
		Events: events,
	}

	e.sendMu.Lock()
	defer e.sendMu.Unlock()

	if err := enc.Encode(&millproto.Message{EventBatch: batch}); err != nil {
		return fmt.Errorf("encode event batch (seqno %d..%d): %w", rows[0].Seqno, lastSeqno, err)
	}
	e.sentSeqno = lastSeqno
	return nil
}

func (e *Executor) replay(ackSeqno uint64) error {
	e.connMu.Lock()
	enc := e.enc
	e.connMu.Unlock()
	if enc == nil {
		return nil
	}

	e.flushMu.Lock()
	defer e.flushMu.Unlock()

	e.sendMu.Lock()
	e.sentSeqno = ackSeqno
	e.sendMu.Unlock()

	return e.sendPendingLocked(enc)
}

func (e *Executor) handleAck(ack *millv1.Ack) {
	if ack == nil || ack.GetEpoch() != e.epoch {
		return
	}

	upTo := ack.GetUpToSeqno()
	if upTo == 0 {
		return
	}

	if err := e.deleteOutboxPrefix(upTo); err != nil {
		e.l.Error("delete outbox prefix failed", "upTo", upTo, "err", err)
	}
}

func (e *Executor) subtractOutboxBytes(deleted db.OutboxDeletion) {
	e.outboxBytes = max(0, e.outboxBytes-deleted.Bytes)
}

func parseStatus(raw json.RawMessage) (*tangled.PipelineStatus, bool) {
	var st tangled.PipelineStatus
	if err := json.Unmarshal(raw, &st); err != nil {
		return nil, false
	}
	return &st, true
}

func (e *Executor) deleteOutboxPrefix(upTo uint64) error {
	e.eventMu.Lock()
	defer e.eventMu.Unlock()

	deleted, err := e.db.DeleteOutboxPrefix(upTo)
	if err == nil {
		e.subtractOutboxBytes(deleted)
	}
	return err
}

func parseStatusExitAndError(st *tangled.PipelineStatus) (int64, string) {
	if st == nil {
		return 0, ""
	}
	var exit int64
	if st.ExitCode != nil {
		exit = *st.ExitCode
	}
	var errStr string
	if st.Error != nil {
		errStr = truncateEventString(*st.Error)
	}
	return exit, errStr
}

func decodeEvent(payload []byte, seqno uint64) (*millv1.Event, error) {
	var ev millv1.Event
	if err := proto.Unmarshal(payload, &ev); err != nil {
		return nil, err
	}
	ev.Seqno = seqno
	return &ev, nil
}
