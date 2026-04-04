package migration

import (
	"context"
	"fmt"

	"github.com/bluesky-social/indigo/api/agnostic"
	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/lex/util"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/orm"
)

func (s *Migration) migrateUseFeedComment(ctx context.Context, client *atclient.APIClient, did syntax.DID, record syntax.ATURI) error {
	l := s.logger.With("aturi", record)
	l.Debug("migrating record")

	switch record.Collection() {
	case tangled.RepoIssueCommentNSID:
	case tangled.RepoPullCommentNSID:
	default:
		return fmt.Errorf("unexpected collection: '%s'", record.Collection())
	}

	comment, err := db.GetComment(s.db, orm.FilterEq("at_uri", record))
	if err != nil {
		return fmt.Errorf("db: %w", err)
	}

	comment.Collection = tangled.FeedCommentNSID

	// only update from DB if comment is deleted
	if comment.Deleted != nil {
		l.Info("skipping pds migration for deleted record")

		return nil
	}

	// fill missing reference CIDs
	if comment.Subject.Cid == "" {
		cid, err := s.getRecordCid(ctx, syntax.ATURI(comment.Subject.Uri))
		if err != nil {
			return fmt.Errorf("pds: getRecordCid for subject.uri: %w", err)
		}
		comment.Subject.Cid = cid.String()
	}
	if comment.ReplyTo != nil && comment.ReplyTo.Cid == "" {
		uri, err := syntax.ParseATURI(comment.ReplyTo.Uri)
		if err != nil {
			return fmt.Errorf("invalid replyTo.uri: %w", err)
		}

		// assume parent comment is already migrated to `sh.tangled.feed.comment`.
		// fail if it isn't ready
		uri = syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", uri.Authority(), tangled.FeedCommentNSID, uri.RecordKey()))

		cid, err := s.getRecordCid(ctx, uri)
		if err != nil {
			return fmt.Errorf("pds: getRecordCid for replyTo.uri: %w", err)
		}
		comment.ReplyTo.Uri = uri.String()
		comment.ReplyTo.Cid = cid.String()
	}

	// use same rkey for new record
	rkey := record.RecordKey().String()

	if _, err := comatproto.RepoApplyWrites(ctx, client, &comatproto.RepoApplyWrites_Input{
		Repo: did.String(),
		Writes: []*comatproto.RepoApplyWrites_Input_Writes_Elem{
			{RepoApplyWrites_Delete: &comatproto.RepoApplyWrites_Delete{
				Collection: record.Collection().String(),
				Rkey:       rkey,
			}},
			{RepoApplyWrites_Create: &comatproto.RepoApplyWrites_Create{
				Collection: tangled.FeedCommentNSID,
				Rkey:       &rkey,
				Value:      &util.LexiconTypeDecoder{Val: comment.AsRecord()},
			}},
		},
	}); err != nil {
		return fmt.Errorf("pds: applyWrites: %w", err)
	}

	return nil
}

func (s *Migration) getRecordCid(ctx context.Context, uri syntax.ATURI) (syntax.CID, error) {
	ident, err := s.dir.Lookup(ctx, uri.Authority())
	if err != nil {
		return "", err
	}

	xrpcc := xrpc.Client{Host: ident.PDSEndpoint()}
	out, err := agnostic.RepoGetRecord(ctx, &xrpcc, "", uri.Collection().String(), ident.DID.String(), uri.RecordKey().String())
	if err != nil {
		return "", err
	}
	if out.Cid == nil {
		return "", fmt.Errorf("record CID is empty")
	}

	cid, err := syntax.ParseCID(*out.Cid)
	if err != nil {
		return "", err
	}

	return cid, nil
}
