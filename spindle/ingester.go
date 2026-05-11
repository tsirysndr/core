package spindle

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/db"

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
		}

		if err != nil {
			s.l.Warn("failed to process message, skipping", "nsid", e.Commit.Collection, "err", err)
		}

		lastTimeUs := e.TimeUS + 1
		if saveErr := s.db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
			s.l.Error("failed to save cursor", "err", saveErr)
		}

		return nil
	}
}

func (s *Spindle) ingestMember(_ context.Context, e *models.Event) error {
	var err error
	did := e.Did
	rkey := e.Commit.RKey

	l := s.l.With("component", "ingester", "record", tangled.SpindleMemberNSID)

	switch e.Commit.Operation {
	case models.CommitOperationCreate, models.CommitOperationUpdate:
		raw := e.Commit.Record
		record := tangled.SpindleMember{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "error", err)
			return err
		}

		domain := s.cfg.Server.Hostname
		recordInstance := record.Instance

		if recordInstance != domain {
			l.Error("domain mismatch", "domain", recordInstance, "expected", domain)
			return fmt.Errorf("domain mismatch: %s != %s", record.Instance, domain)
		}

		ok, err := s.e.IsSpindleInviteAllowed(did, rbacDomain)
		if err != nil || !ok {
			l.Error("failed to add member", "did", did, "error", err)
			return fmt.Errorf("failed to enforce permissions: %w", err)
		}

		if err := db.AddSpindleMember(s.db, db.SpindleMember{
			Did:      syntax.DID(did),
			Rkey:     rkey,
			Instance: recordInstance,
			Subject:  syntax.DID(record.Subject),
			Created:  time.Now(),
		}); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}

		if err := s.e.AddSpindleMember(rbacDomain, record.Subject); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}
		l.Info("added member from firehose", "member", record.Subject)

		if err := s.db.AddDid(record.Subject); err != nil {
			l.Error("failed to add did", "error", err)
			return fmt.Errorf("failed to add did: %w", err)
		}
		s.jc.AddDid(record.Subject)

		return nil

	case models.CommitOperationDelete:
		record, err := db.GetSpindleMember(s.db, did, rkey)
		if err != nil {
			l.Error("failed to find member", "error", err)
			return fmt.Errorf("failed to find member: %w", err)
		}

		if err := db.RemoveSpindleMember(s.db, did, rkey); err != nil {
			l.Error("failed to remove member", "error", err)
			return fmt.Errorf("failed to remove member: %w", err)
		}

		if err := s.e.RemoveSpindleMember(rbacDomain, record.Subject.String()); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}
		l.Info("added member from firehose", "member", record.Subject)

		if err := s.db.RemoveDid(record.Subject.String()); err != nil {
			l.Error("failed to add did", "error", err)
			return fmt.Errorf("failed to add did: %w", err)
		}
		s.jc.RemoveDid(record.Subject.String())

	}
	return nil
}
