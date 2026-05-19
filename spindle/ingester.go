package spindle

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/db"
	"tangled.org/core/tapc"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/jetstream/pkg/models"
)

type Ingester func(ctx context.Context, e *models.Event) error

func (s *Spindle) ingest() Ingester {
	return func(ctx context.Context, e *models.Event) error {
		if e.Kind != models.EventKindCommit {
			return nil
		}

		var err error
		switch e.Commit.Collection {
		case tangled.SpindleMemberNSID:
			err = s.ingestMember(ctx, e)
		case tangled.RepoNSID, tangled.RepoCollaboratorNSID:
			if evt, ok := jetstreamToTapEvent(e); ok {
				err = s.tap.processEvent(ctx, evt)
			}
		}

		if err != nil {
			s.l.Warn("failed to process message, skipping", "nsid", e.Commit.Collection, "did", e.Did, "rkey", e.Commit.RKey, "err", err)
		}

		lastTimeUs := e.TimeUS + 1
		if saveErr := s.db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
			s.l.Error("failed to save cursor", "err", saveErr)
		}

		return nil
	}
}

func jetstreamToTapEvent(e *models.Event) (tapc.Event, bool) {
	if e.Commit == nil {
		return tapc.Event{}, false
	}
	did, err := syntax.ParseDID(e.Did)
	if err != nil {
		return tapc.Event{}, false
	}
	var action tapc.RecordAction
	switch e.Commit.Operation {
	case models.CommitOperationCreate:
		action = tapc.RecordCreateAction
	case models.CommitOperationUpdate:
		action = tapc.RecordUpdateAction
	case models.CommitOperationDelete:
		action = tapc.RecordDeleteAction
	default:
		return tapc.Event{}, false
	}
	return tapc.Event{
		Type: tapc.EvtRecord,
		Record: &tapc.RecordEventData{
			Did:        did,
			Rkey:       syntax.RecordKey(e.Commit.RKey),
			Collection: syntax.NSID(e.Commit.Collection),
			Action:     action,
			Record:     e.Commit.Record,
		},
	}, true
}

func (s *Spindle) ingestMember(ctx context.Context, e *models.Event) error {
	did := e.Did
	rkey := e.Commit.RKey
	l := s.l.With("component", "ingester", "record", tangled.SpindleMemberNSID, "did", did, "rkey", rkey)

	switch e.Commit.Operation {
	case models.CommitOperationCreate, models.CommitOperationUpdate:
		raw := e.Commit.Record
		record := tangled.SpindleMember{}
		if err := json.Unmarshal(raw, &record); err != nil {
			return fmt.Errorf("invalid record: %w", err)
		}

		domain := s.cfg.Server.Hostname
		recordInstance := record.Instance

		if recordInstance != domain {
			return fmt.Errorf("domain mismatch: %s != %s", record.Instance, domain)
		}

		subject, err := syntax.ParseDID(record.Subject)
		if err != nil {
			return fmt.Errorf("invalid subject DID %q: %w", record.Subject, err)
		}

		ok, err := s.e.IsSpindleInviteAllowed(did, rbacDomain)
		if err != nil {
			return fmt.Errorf("failed to enforce permissions: %w", err)
		}
		if !ok {
			return fmt.Errorf("permission denied for %s", did)
		}

		sqlTx, err := s.db.BeginTx(ctx, nil)
		if err != nil {
			return fmt.Errorf("failed to start txn: %w", err)
		}
		committed := false
		defer func() {
			if !committed {
				sqlTx.Rollback()
			}
		}()

		existing, err := db.GetSpindleMember(sqlTx, did, rkey)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return fmt.Errorf("failed to look up existing member: %w", err)
		}

		var staleSubject string
		if existing != nil && existing.Subject != subject {
			staleSubject = existing.Subject.String()
			if err := db.RemoveSpindleMember(sqlTx, did, rkey); err != nil {
				return fmt.Errorf("failed to remove stale member row: %w", err)
			}
		}

		if err := db.AddSpindleMember(sqlTx, db.SpindleMember{
			Did:      syntax.DID(did),
			Rkey:     rkey,
			Instance: recordInstance,
			Subject:  subject,
		}); err != nil {
			return fmt.Errorf("failed to add member: %w", err)
		}

		if err := db.AddDid(sqlTx, subject.String()); err != nil {
			return fmt.Errorf("failed to add did: %w", err)
		}

		dropStaleAcl := false
		var staleDidDropped bool
		if staleSubject != "" {
			remaining, err := db.CountSpindleMembersBySubject(sqlTx, staleSubject)
			if err != nil {
				return fmt.Errorf("failed to count stale subject rows: %w", err)
			}
			if remaining == 0 {
				dropStaleAcl = true
				stillNeeded, err := s.e.WouldHaveAnyPolicyExcludingSpindleMember(staleSubject, rbacDomain)
				if err != nil {
					return fmt.Errorf("failed to check residual policies for stale subject: %w", err)
				}
				if !stillNeeded {
					if err := db.RemoveDid(sqlTx, staleSubject); err != nil {
						return fmt.Errorf("failed to remove stale did: %w", err)
					}
					staleDidDropped = true
				}
			}
			l.Info("replaced stale spindle member", "old_subject", staleSubject, "new_subject", subject, "stale_did_dropped", staleDidDropped)
		}

		if err := sqlTx.Commit(); err != nil {
			return fmt.Errorf("failed to commit txn: %w", err)
		}
		committed = true

		if dropStaleAcl {
			if _, err := s.e.TryRemoveSpindleMember(rbacDomain, staleSubject); err != nil {
				l.Error("post-commit: failed to remove stale ACL", "subject", staleSubject, "err", err)
			}
		}
		if _, err := s.e.TryAddSpindleMember(rbacDomain, subject.String()); err != nil {
			l.Error("post-commit: failed to add member ACL", "subject", subject, "err", err)
		}

		if staleDidDropped {
			s.jc.RemoveDid(staleSubject)
		}
		s.jc.AddDid(subject.String())
		l.Info("added member from firehose", "member", subject)
		return nil

	case models.CommitOperationDelete:
		sqlTx, err := s.db.BeginTx(ctx, nil)
		if err != nil {
			return fmt.Errorf("failed to start txn: %w", err)
		}
		committed := false
		defer func() {
			if !committed {
				sqlTx.Rollback()
			}
		}()

		record, err := db.GetSpindleMember(sqlTx, did, rkey)
		if errors.Is(err, sql.ErrNoRows) {
			l.Info("spindle member already removed")
			return nil
		}
		if err != nil {
			return fmt.Errorf("failed to find member: %w", err)
		}

		staleSubject := record.Subject.String()

		if err := db.RemoveSpindleMember(sqlTx, did, rkey); err != nil {
			return fmt.Errorf("failed to remove member: %w", err)
		}

		remaining, err := db.CountSpindleMembersBySubject(sqlTx, staleSubject)
		if err != nil {
			return fmt.Errorf("failed to count remaining member rows: %w", err)
		}

		dropAcl := false
		var staleDidDropped bool
		if remaining == 0 {
			dropAcl = true
			stillNeeded, err := s.e.WouldHaveAnyPolicyExcludingSpindleMember(staleSubject, rbacDomain)
			if err != nil {
				return fmt.Errorf("failed to check residual policies: %w", err)
			}
			if !stillNeeded {
				if err := db.RemoveDid(sqlTx, staleSubject); err != nil {
					return fmt.Errorf("failed to remove did: %w", err)
				}
				staleDidDropped = true
			}
		}

		if err := sqlTx.Commit(); err != nil {
			return fmt.Errorf("failed to commit txn: %w", err)
		}
		committed = true

		if dropAcl {
			if _, err := s.e.TryRemoveSpindleMember(rbacDomain, staleSubject); err != nil {
				l.Error("post-commit: failed to remove member ACL", "subject", staleSubject, "err", err)
			}
		}

		if staleDidDropped {
			s.jc.RemoveDid(staleSubject)
		}
		l.Info("removed member from firehose", "member", record.Subject, "remaining_rows", remaining, "stale_did_dropped", staleDidDropped)
	}
	return nil
}
