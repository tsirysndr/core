package migration

import (
	"bytes"
	"context"
	"fmt"

	"github.com/bluesky-social/indigo/api/agnostic"
	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/lex/util"
	"github.com/bluesky-social/indigo/xrpc"
	"github.com/ipfs/go-cid"
	"github.com/multiformats/go-multihash"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
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

	comments, err := db.GetComments(s.db, orm.FilterEq("at_uri", record))
	if err != nil {
		return fmt.Errorf("db: %w", err)
	}
	if len(comments) < 1 {
		l.Info("can't found legacy record from db. skipping migration")
		return nil
	}
	comment := comments[0]

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

		cid, err := s.guessParentCommentCid(uri, &comment)
		if err != nil {
			return fmt.Errorf("cbor: guessParentCommentCid for replyTo.uri: %w", err)
		}
		comment.ReplyTo.Uri = uri.String()
		comment.ReplyTo.Cid = cid.String()
	}

	// use same rkey for new record
	rkey := record.RecordKey().String()

	// ensure new record is missing in PDS
	if _, err := agnostic.RepoGetRecord(ctx, client, "", tangled.FeedCommentNSID, did.String(), rkey); err == nil {
		l.Info("New comment record already exists")
	} else {
		// insert new record
		if _, err := comatproto.RepoCreateRecord(ctx, client, &comatproto.RepoCreateRecord_Input{
			Repo:       did.String(),
			Collection: tangled.FeedCommentNSID,
			Rkey:       &rkey,
			Record:     &util.LexiconTypeDecoder{Val: comment.AsRecord()},
		}); err != nil {
			return fmt.Errorf("pds: putRecord: %w", err)
		}
	}

	if _, err := comatproto.RepoDeleteRecord(ctx, client, &comatproto.RepoDeleteRecord_Input{
		Repo:       did.String(),
		Collection: record.Collection().String(),
		Rkey:       rkey,
	}); err != nil {
		l.Info("Failed to cleanup old record. Proceeding migration...", "err", err)
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

func (s *Migration) guessParentCommentCid(uri syntax.ATURI, comment *models.Comment) (syntax.CID, error) {
	parent, err := db.GetComment(s.db, orm.FilterEq("did", uri.Authority()), orm.FilterEq("rkey", uri.RecordKey()))
	if err != nil {
		return "", fmt.Errorf("db: failed to queyr subject comment: %w", err)
	}
	if parent.Deleted != nil {
		// leave cid empty. reply comment won't pass the schema validation.
		return "", nil
	}

	// since parent comment is also migrating, parent comments subject.cid might be empty
	if parent.Subject.Cid == "" {
		parent.Subject.Cid = comment.Subject.Cid
	}

	buf := new(bytes.Buffer)
	if err := parent.AsRecord().MarshalCBOR(buf); err != nil {
		return "", fmt.Errorf("MarshalCBOR: %w", err)
	}
	c, err := cid.NewPrefixV1(cid.DagCBOR, multihash.SHA2_256).Sum(buf.Bytes())
	if err != nil {
		return "", fmt.Errorf("cid: sum: %w", err)
	}
	return syntax.CID(c.String()), nil
}
