package db

import (
	"database/sql"
	"fmt"

	"tangled.org/core/eventstream"
	"tangled.org/core/notifier"
)

// enough to rebuild the fencing token and workflow identity after a restart
type MillLease struct {
	LeaseID  string
	NodeID   string
	Epoch    string
	Engine   string
	Knot     string
	Rkey     string
	Workflow string
	State    string
}

type ExecutorCursor struct {
	NodeID     string
	Epoch      string
	AckedSeqno uint64
}

type OutboxRow struct {
	Epoch    string
	Seqno    uint64
	Payload  []byte
	ByteSize int64
	Control  bool
}
type OutboxDeletion struct {
	Rows  int64
	Bytes int64
}

func (d *DB) SaveMillLease(l MillLease) error {
	_, err := d.Exec(
		`insert into mill_leases (
			lease_id, node_id, epoch, engine, knot, rkey, workflow, state
		) values (?, ?, ?, ?, ?, ?, ?, ?)
		on conflict(lease_id) do update set state = excluded.state`,
		l.LeaseID, l.NodeID, l.Epoch, l.Engine, l.Knot, l.Rkey, l.Workflow, l.State,
	)
	return err
}

func (d *DB) DeleteMillLease(leaseID string) error {
	_, err := d.Exec(`delete from mill_leases where lease_id = ?`, leaseID)
	return err
}

func (d *DB) ListMillLeases() ([]MillLease, error) {
	rows, err := d.Query(`
		select lease_id, node_id, epoch, engine, knot, rkey, workflow, state
		from mill_leases
	`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var leases []MillLease
	for rows.Next() {
		var l MillLease
		if err := rows.Scan(
			&l.LeaseID, &l.NodeID, &l.Epoch, &l.Engine, &l.Knot, &l.Rkey, &l.Workflow, &l.State,
		); err != nil {
			return nil, err
		}
		leases = append(leases, l)
	}
	return leases, rows.Err()
}

func (d *DB) ListExecutorCursors() ([]ExecutorCursor, error) {
	rows, err := d.Query(`select node_id, epoch, acked_seqno from mill_executor_cursors`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var cursors []ExecutorCursor
	for rows.Next() {
		var c ExecutorCursor
		if err := rows.Scan(&c.NodeID, &c.Epoch, &c.AckedSeqno); err != nil {
			return nil, err
		}
		cursors = append(cursors, c)
	}
	return cursors, rows.Err()
}

func (d *DB) SetOutboxEpoch(epoch string) error {
	tx, err := d.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if _, err := tx.Exec(`delete from mill_outbox_rows`); err != nil {
		return err
	}
	if _, err := tx.Exec(`delete from mill_outbox_state`); err != nil {
		return err
	}
	if _, err := tx.Exec(`insert into mill_outbox_state (epoch, next_seqno) values (?, 1)`, epoch); err != nil {
		return err
	}
	return tx.Commit()
}

func (d *DB) AppendOutboxRow(payload []byte, control bool) (uint64, error) {
	tx, err := d.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()

	var epoch string
	var nextSeqno uint64
	err = tx.QueryRow(`select epoch, next_seqno from mill_outbox_state limit 1`).Scan(&epoch, &nextSeqno)
	if err == sql.ErrNoRows {
		return 0, fmt.Errorf("no outbox epoch set")
	} else if err != nil {
		return 0, err
	}

	byteSize := int64(len(payload))
	controlVal := 0
	if control {
		controlVal = 1
	}

	if _, err := tx.Exec(
		`insert into mill_outbox_rows (epoch, seqno, payload, byte_size, control) values (?, ?, ?, ?, ?)`,
		epoch, nextSeqno, payload, byteSize, controlVal,
	); err != nil {
		return 0, err
	}

	if _, err := tx.Exec(
		`update mill_outbox_state set next_seqno = ? where epoch = ?`,
		nextSeqno+1, epoch,
	); err != nil {
		return 0, err
	}

	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return nextSeqno, nil
}

func (d *DB) DeleteOutboxPrefix(ackedSeqno uint64) (OutboxDeletion, error) {
	tx, err := d.Begin()
	if err != nil {
		return OutboxDeletion{}, err
	}
	defer tx.Rollback()

	var epoch string
	err = tx.QueryRow(`select epoch from mill_outbox_state limit 1`).Scan(&epoch)
	if err == sql.ErrNoRows {
		return OutboxDeletion{}, nil
	}
	if err != nil {
		return OutboxDeletion{}, err
	}

	var deleted OutboxDeletion
	if err := tx.QueryRow(`
		select count(*),
		       coalesce(sum(byte_size), 0)
		from mill_outbox_rows
		where epoch = ? and seqno <= ?
	`, epoch, ackedSeqno).Scan(&deleted.Rows, &deleted.Bytes); err != nil {
		return OutboxDeletion{}, err
	}
	if _, err := tx.Exec(
		`delete from mill_outbox_rows where epoch = ? and seqno <= ?`,
		epoch, ackedSeqno,
	); err != nil {
		return OutboxDeletion{}, err
	}
	if err := tx.Commit(); err != nil {
		return OutboxDeletion{}, err
	}
	return deleted, nil
}

func (d *DB) ListOutboxRows() ([]OutboxRow, error) {
	rows, err := d.Query(`select epoch, seqno, payload, byte_size, control from mill_outbox_rows order by seqno`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []OutboxRow
	for rows.Next() {
		var r OutboxRow
		var controlVal int
		if err := rows.Scan(&r.Epoch, &r.Seqno, &r.Payload, &r.ByteSize, &controlVal); err != nil {
			return nil, err
		}
		r.Control = (controlVal != 0)
		out = append(out, r)
	}
	return out, rows.Err()
}
func (d *DB) ListOutboxRowsAfter(seqno uint64, limit int) ([]OutboxRow, error) {
	rows, err := d.Query(`
		select epoch, seqno, payload, byte_size, control
		from mill_outbox_rows
		where epoch = (select epoch from mill_outbox_state limit 1)
		  and seqno > ?
		order by seqno
		limit ?
	`, seqno, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []OutboxRow
	for rows.Next() {
		var row OutboxRow
		var control int
		if err := rows.Scan(&row.Epoch, &row.Seqno, &row.Payload, &row.ByteSize, &control); err != nil {
			return nil, err
		}
		row.Control = control != 0
		out = append(out, row)
	}
	return out, rows.Err()
}

func (d *DB) GetOutboxState() (string, uint64, error) {
	var epoch string
	var nextSeqno uint64
	err := d.QueryRow(`select epoch, next_seqno from mill_outbox_state limit 1`).Scan(&epoch, &nextSeqno)
	if err == sql.ErrNoRows {
		return "", 0, nil
	}
	return epoch, nextSeqno, err
}

type EventBatchTx struct {
	tx *sql.Tx
	db *DB
}

func (tx *EventBatchTx) InsertStatusEvent(pipelineAtUri, workflow, status string, workflowError *string, exitCode *int64) error {
	event, err := statusEvent(pipelineAtUri, workflow, status, workflowError, exitCode)
	if err != nil {
		return err
	}
	return eventstream.Insert(tx.tx, event, nil)
}

func (tx *EventBatchTx) DeleteLease(leaseID string) error {
	_, err := tx.tx.Exec(`delete from mill_leases where lease_id = ?`, leaseID)
	return err
}

func (tx *EventBatchTx) AdvanceCursor(nodeID, epoch string, seqno uint64) error {
	_, err := tx.tx.Exec(
		`insert into mill_executor_cursors (node_id, epoch, acked_seqno) values (?, ?, ?)
		 on conflict(node_id, epoch) do update set acked_seqno = excluded.acked_seqno`,
		nodeID, epoch, seqno,
	)
	return err
}

// start from zero if a row is missing
func (d *DB) GetExecutorCursor(nodeID, epoch string) (uint64, error) {
	var seqno uint64
	err := d.QueryRow(
		`select acked_seqno from mill_executor_cursors where node_id = ? and epoch = ?`,
		nodeID, epoch,
	).Scan(&seqno)
	if err == sql.ErrNoRows {
		return 0, nil
	}
	return seqno, err
}

// forgets everything the mill applied from this node, picked up on the
// executor's next reconnect since cursors are re-read at session start
func (d *DB) DeleteExecutorCursors(nodeID string) (int64, error) {
	res, err := d.Exec(`delete from mill_executor_cursors where node_id = ?`, nodeID)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// marks everything at or below seqno as applied, for skipping a backlog
// the mill can never receive
func (d *DB) SetExecutorCursors(nodeID string, seqno uint64) (int64, error) {
	res, err := d.Exec(`update mill_executor_cursors set acked_seqno = ? where node_id = ?`, seqno, nodeID)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// clears artifacts waiting on leases from the old stream, replaying them
// under a new epoch would trip the mill's lease-epoch check
func (d *DB) ClearPendingArtifacts() error {
	_, err := d.Exec(`delete from executor_pending_artifacts`)
	return err
}

func (tx *EventBatchTx) InsertArtifactRef(leaseID, workflow, ref, hash string) error {
	_, err := tx.tx.Exec(
		`insert into mill_artifacts (lease_id, workflow, ref, hash)
		 values (?, ?, ?, ?)`,
		leaseID, workflow, ref, hash,
	)
	return err
}

type PendingArtifact struct {
	LeaseID  string
	Workflow string
	Status   string
	Error    string
	ExitCode int64
	Ref      string
	Hash     string
}

func (d *DB) SavePendingArtifact(leaseID, workflow, status, errStr string, exitCode int64, ref, hash string) error {
	_, err := d.Exec(
		`insert into executor_pending_artifacts (lease_id, workflow, status, error, exit_code, ref, hash)
		 values (?, ?, ?, ?, ?, ?, ?)
		 on conflict(lease_id) do update set
			workflow = excluded.workflow,
			status = excluded.status,
			error = excluded.error,
			exit_code = excluded.exit_code,
			ref = excluded.ref,
			hash = excluded.hash`,
		leaseID, workflow, status, errStr, exitCode, ref, hash,
	)
	return err
}

func (d *DB) RemovePendingArtifact(leaseID string) error {
	_, err := d.Exec(`delete from executor_pending_artifacts where lease_id = ?`, leaseID)
	return err
}

func (d *DB) ListPendingArtifacts() ([]PendingArtifact, error) {
	rows, err := d.Query(`select lease_id, workflow, status, error, exit_code, ref, hash from executor_pending_artifacts`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var res []PendingArtifact
	for rows.Next() {
		var p PendingArtifact
		if err := rows.Scan(&p.LeaseID, &p.Workflow, &p.Status, &p.Error, &p.ExitCode, &p.Ref, &p.Hash); err != nil {
			return nil, err
		}
		res = append(res, p)
	}
	return res, rows.Err()
}

func (d *DB) ApplyEventBatch(n *notifier.Notifier, fn func(tx *EventBatchTx) error) error {
	tx, err := d.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	batchTx := &EventBatchTx{
		tx: tx,
		db: d,
	}

	if err := fn(batchTx); err != nil {
		return err
	}

	if err := tx.Commit(); err != nil {
		return err
	}

	if n != nil {
		n.NotifyAll()
	}
	return nil
}
