package migration

import (
	"context"
	"errors"
	"fmt"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/samber/lo"
	cbg "github.com/whyrusleeping/cbor-gen"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
)

func (s *Migration) backfillEntityState(ctx context.Context, client *atclient.APIClient, did syntax.DID, _ syntax.ATURI) error {
	l := s.logger.With("migration", db.EntityStateBackfillName, "owner", did)

	subjects, err := db.ColumnOnlyClosedSubjectsForOwner(ctx, s.db, did)
	if err != nil {
		return fmt.Errorf("db: %w", err)
	}
	if len(subjects) == 0 {
		l.Info("no column-only closed entity state to backfill")
		return nil
	}

	results := lo.Map(subjects, func(subj db.BackfillSubject, _ int) error {
		if err := s.writeBackfillStateRecord(ctx, client, did, subj); err != nil {
			l.Error("failed to backfill entity state record", "subject", subj.Subject, "err", err)
			return err
		}
		return nil
	})

	written := lo.CountBy(results, func(err error) bool { return err == nil })
	l.Info("backfilled entity state", "written", written, "total", len(subjects))

	return errors.Join(results...)
}

func (s *Migration) writeBackfillStateRecord(ctx context.Context, client *atclient.APIClient, owner syntax.DID, subj db.BackfillSubject) error {
	createdAt, err := time.Parse(time.RFC3339, subj.CreatedAt)
	if err != nil {
		createdAt = time.Unix(0, 0).UTC()
	}

	collection, record, err := backfillRecord(subj, createdAt)
	if err != nil {
		return err
	}

	_, err = comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Repo:       owner.String(),
		Collection: collection,
		Rkey:       subj.Subject.RecordKey().String(),
		Record:     &lexutil.LexiconTypeDecoder{Val: record},
	})
	return err
}

func backfillRecord(subj db.BackfillSubject, createdAt time.Time) (string, cbg.CBORMarshaler, error) {
	switch subj.Subject.Collection().String() {
	case tangled.RepoIssueNSID:
		record, err := models.AsIssueStateRecord(subj.Subject, subj.Value, createdAt)
		if err != nil {
			return "", nil, err
		}
		return tangled.RepoIssueStateNSID, &record, nil
	case tangled.RepoPullNSID:
		records, err := models.AsPullStatusRecords([]syntax.ATURI{subj.Subject}, subj.Value, createdAt)
		if err != nil {
			return "", nil, err
		}
		return tangled.RepoPullStatusNSID, &records[0], nil
	default:
		return "", nil, fmt.Errorf("unexpected backfill subject collection: %s", subj.Subject.Collection())
	}
}
