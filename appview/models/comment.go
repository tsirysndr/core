package models

import (
	"fmt"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	typegen "github.com/whyrusleeping/cbor-gen"
	"tangled.org/core/api/tangled"
)

type Comment struct {
	Id int64

	Did        syntax.DID
	Collection syntax.NSID
	Rkey       syntax.RecordKey
	Cid        syntax.CID

	// record content
	Subject      comatproto.RepoStrongRef
	Body         tangled.MarkupMarkdown // markup body type. only markdown is supported right now
	Created      time.Time
	ReplyTo      *comatproto.RepoStrongRef // (optional) parent comment
	PullRoundIdx *int                      // (optional) pull round number used when subject is sh.tangled.repo.pull

	// store on db, but not on PDS
	Edited  *time.Time
	Deleted *time.Time
}

func (c *Comment) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", c.Did, c.Collection, c.Rkey))
}

func (c *Comment) StrongRef() comatproto.RepoStrongRef {
	return comatproto.RepoStrongRef{
		Uri: c.AtUri().String(),
		Cid: c.Cid.String(),
	}
}

func (c *Comment) AsRecord() typegen.CBORMarshaler {
	// can't convert to record for legacy types
	if c.Collection != tangled.FeedCommentNSID {
		return nil
	}
	var pullRoundIdx *int64
	if c.PullRoundIdx != nil {
		pullRoundIdx = new(int64)
		*pullRoundIdx = int64(*c.PullRoundIdx)
	}
	return &tangled.FeedComment{
		Subject:      &c.Subject,
		Body:         &tangled.FeedComment_Body{MarkupMarkdown: &c.Body},
		CreatedAt:    c.Created.Format(time.RFC3339),
		ReplyTo:      c.ReplyTo,
		PullRoundIdx: pullRoundIdx,
	}
}

func (c *Comment) IsTopLevel() bool {
	return c.ReplyTo == nil
}

func (c *Comment) IsReply() bool {
	return c.ReplyTo != nil
}

func (c *Comment) Validate() error {
	// TODO: sanitize the body and then trim space
	if sb := strings.TrimSpace(c.Body.Text); sb == "" {
		return fmt.Errorf("body is empty after HTML sanitization")
	}

	// if it's for PR, PullSubmissionId should not be nil
	subjectAt, err := syntax.ParseATURI(c.Subject.Uri)
	if err != nil {
		return fmt.Errorf("subject.uri is not valid at-uri: %w", err)
	}
	if subjectAt.Collection().String() == tangled.RepoPullNSID {
		if c.PullRoundIdx == nil {
			return fmt.Errorf("pullSubmissionId should not be nil when subject is sh.tangled.repo.pull")
		}
	}
	return nil
}

func CommentFromRecord(did syntax.DID, rkey syntax.RecordKey, cid syntax.CID, record tangled.FeedComment) (*Comment, error) {
	created, err := time.Parse(time.RFC3339, record.CreatedAt)
	if err != nil {
		created = time.Now()
	}

	if record.Subject == nil {
		return nil, fmt.Errorf("subject can't be nil")
	}
	subjectAt, err := syntax.ParseATURI(record.Subject.Uri)
	if err != nil {
		return nil, fmt.Errorf("invalid subject uri: %w", err)
	}
	if _, err = syntax.ParseCID(record.Subject.Cid); err != nil {
		return nil, fmt.Errorf("invalid subject cid: %w", err)
	}

	if subjectAt.Collection() == tangled.RepoPullNSID {
		if record.PullRoundIdx == nil {
			return nil, fmt.Errorf("pullRoundIdx can't be nil when subject is sh.tangled.repo.pull")
		}
	}

	if record.Body == nil {
		return nil, fmt.Errorf("body can't be nil")
	}
	if record.Body.MarkupMarkdown == nil {
		return nil, fmt.Errorf("body should be markdown type")
	}

	if record.ReplyTo != nil {
		if _, err = syntax.ParseATURI(record.ReplyTo.Uri); err != nil {
			return nil, fmt.Errorf("invalid replyTo uri: %w", err)
		}
		if _, err = syntax.ParseCID(record.ReplyTo.Cid); err != nil {
			return nil, fmt.Errorf("invalid replyTo cid: %w", err)
		}
	}

	var pullRoundIdx *int
	if record.PullRoundIdx != nil {
		pullRoundIdx = new(int)
		*pullRoundIdx = int(*record.PullRoundIdx)
	}

	return &Comment{
		Did:        did,
		Collection: tangled.FeedCommentNSID,
		Rkey:       rkey,
		Cid:        cid,

		Subject:      *record.Subject,
		Body:         *record.Body.MarkupMarkdown,
		Created:      created,
		ReplyTo:      record.ReplyTo,
		PullRoundIdx: pullRoundIdx,
	}, nil
}
