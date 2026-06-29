package db

import (
	"database/sql"
	"errors"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
)

type stateTable struct {
	name     string
	valueCol string
}

var (
	issueStateTable = stateTable{name: "issue_states", valueCol: "state"}
	pullStateTable  = stateTable{name: "pull_states", valueCol: "status"}
)

func putStateRecord(tx *sql.Tx, t stateTable, rec models.StateRecord) (syntax.ATURI, error) {
	var priorSubject string
	err := tx.QueryRow(
		fmt.Sprintf(`select subject from %s where did = ? and rkey = ?`, t.name),
		rec.Did, rec.Rkey,
	).Scan(&priorSubject)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		priorSubject = ""
	case err != nil:
		return "", err
	}

	if _, err := tx.Exec(fmt.Sprintf(`
		insert into %s (did, rkey, subject, %s, created_micros)
		values (?, ?, ?, ?, ?)
		on conflict(did, rkey) do update set
			subject = excluded.subject,
			%s = excluded.%s,
			created_micros = excluded.created_micros
	`, t.name, t.valueCol, t.valueCol, t.valueCol),
		rec.Did, rec.Rkey, rec.Subject, string(rec.Value), rec.SortMicros); err != nil {
		return "", err
	}

	if priorSubject != "" && syntax.ATURI(priorSubject) != rec.Subject {
		return syntax.ATURI(priorSubject), nil
	}
	return "", nil
}

func deleteStateRecord(tx *sql.Tx, t stateTable, did, rkey string) (syntax.ATURI, error) {
	var subject string
	err := tx.QueryRow(
		fmt.Sprintf(`select subject from %s where did = ? and rkey = ?`, t.name),
		did, rkey,
	).Scan(&subject)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		return "", nil
	case err != nil:
		return "", err
	}

	if _, err := tx.Exec(
		fmt.Sprintf(`delete from %s where did = ? and rkey = ?`, t.name),
		did, rkey,
	); err != nil {
		return "", err
	}
	return syntax.ATURI(subject), nil
}

func stateWinner(e Execer, t stateTable, subject syntax.ATURI) (models.StateValue, bool, error) {
	var v string
	err := e.QueryRow(fmt.Sprintf(`
		select %s from %s
		where subject = ?
		order by created_micros desc, at_uri desc
		limit 1
	`, t.valueCol, t.name), subject).Scan(&v)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		return "", false, nil
	case err != nil:
		return "", false, err
	}
	return models.StateValue(v), true, nil
}

func PutIssueState(tx *sql.Tx, rec models.StateRecord) (syntax.ATURI, error) {
	return putStateRecord(tx, issueStateTable, rec)
}

func PutPullStatus(tx *sql.Tx, rec models.StateRecord) (syntax.ATURI, error) {
	return putStateRecord(tx, pullStateTable, rec)
}

type PendingStateRecord struct {
	Did     string
	Rkey    string
	Nsid    string
	Subject syntax.ATURI
	Record  []byte
}

func ParkStateRecord(tx *sql.Tx, p PendingStateRecord) error {
	_, err := tx.Exec(`
		insert into pending_state_records (did, rkey, nsid, subject, record)
		values (?, ?, ?, ?, ?)
		on conflict(did, rkey, nsid) do update set
			subject = excluded.subject,
			record = excluded.record
	`, p.Did, p.Rkey, p.Nsid, p.Subject, p.Record)
	return err
}

func UnparkStateRecord(tx *sql.Tx, did, rkey, nsid string) error {
	_, err := tx.Exec(
		`delete from pending_state_records where did = ? and rkey = ? and nsid = ?`,
		did, rkey, nsid,
	)
	return err
}

func DistinctPendingStateSubjects(e Execer) ([]syntax.ATURI, error) {
	rows, err := e.Query(`select distinct subject from pending_state_records order by subject asc`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var subjects []syntax.ATURI
	for rows.Next() {
		var s string
		if err := rows.Scan(&s); err != nil {
			return nil, err
		}
		subjects = append(subjects, syntax.ATURI(s))
	}
	return subjects, rows.Err()
}

func EvictStalePendingStateRecords(e Execer, before string) (int64, error) {
	res, err := e.Exec(`
		delete from pending_state_records
		where created < ?
		and not exists (select 1 from issues where at_uri = pending_state_records.subject)
		and not exists (select 1 from pulls where at_uri = pending_state_records.subject)
	`, before)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

func PendingStateRecordsForSubject(e Execer, subject syntax.ATURI) ([]PendingStateRecord, error) {
	rows, err := e.Query(
		`select did, rkey, nsid, subject, record from pending_state_records where subject = ? order by id asc`,
		subject,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var pending []PendingStateRecord
	for rows.Next() {
		var p PendingStateRecord
		var subj string
		if err := rows.Scan(&p.Did, &p.Rkey, &p.Nsid, &subj, &p.Record); err != nil {
			return nil, err
		}
		p.Subject = syntax.ATURI(subj)
		pending = append(pending, p)
	}
	return pending, rows.Err()
}

func DeleteIssueState(tx *sql.Tx, did, rkey string) (syntax.ATURI, error) {
	return deleteStateRecord(tx, issueStateTable, did, rkey)
}

func DeletePullStatus(tx *sql.Tx, did, rkey string) (syntax.ATURI, error) {
	return deleteStateRecord(tx, pullStateTable, did, rkey)
}

func setIssueOpen(tx *sql.Tx, subject syntax.ATURI, open bool) error {
	v := 0
	if open {
		v = 1
	}
	_, err := tx.Exec(`update issues set open = ? where at_uri = ?`, v, subject)
	return err
}

func applyIssueState(tx *sql.Tx, subject syntax.ATURI, resetWhenEmpty bool) error {
	winner, ok, err := stateWinner(tx, issueStateTable, subject)
	if err != nil {
		return err
	}
	if !ok {
		if resetWhenEmpty {
			return setIssueOpen(tx, subject, true)
		}
		return nil
	}
	return setIssueOpen(tx, subject, winner == models.StateOpen)
}

func ResolveIssueState(tx *sql.Tx, subject syntax.ATURI) error {
	return applyIssueState(tx, subject, false)
}

func RecomputeIssueState(tx *sql.Tx, subject syntax.ATURI) error {
	return applyIssueState(tx, subject, true)
}

func pullStateFromValue(v models.StateValue) models.PullState {
	switch v {
	case models.StateMerged:
		return models.PullMerged
	case models.StateClosed:
		return models.PullClosed
	default:
		return models.PullOpen
	}
}

func setPullState(tx *sql.Tx, subject syntax.ATURI, st models.PullState) error {
	_, err := tx.Exec(
		`update pulls set state = ? where at_uri = ? and state <> ?`,
		st, subject, models.PullAbandoned,
	)
	return err
}

func applyPullStatus(tx *sql.Tx, subject syntax.ATURI, resetWhenEmpty bool) error {
	winner, ok, err := stateWinner(tx, pullStateTable, subject)
	if err != nil {
		return err
	}
	if !ok {
		if resetWhenEmpty {
			return setPullState(tx, subject, models.PullOpen)
		}
		return nil
	}
	return setPullState(tx, subject, pullStateFromValue(winner))
}

func ResolvePullStatus(tx *sql.Tx, subject syntax.ATURI) error {
	return applyPullStatus(tx, subject, false)
}

func RecomputePullStatus(tx *sql.Tx, subject syntax.ATURI) error {
	return applyPullStatus(tx, subject, true)
}
