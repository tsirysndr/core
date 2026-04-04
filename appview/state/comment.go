package state

import (
	"bytes"
	"fmt"
	"net/http"
	"strconv"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/ipfs/go-cid"
	"github.com/multiformats/go-multihash"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/orm"
	"tangled.org/core/tid"
)

func (s *State) CommentBodyFragment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "CommentBodyFragment")
	user := s.oauth.GetMultiAccountUser(r)

	commentAt := r.URL.Query().Get("aturi")
	comment, err := db.GetComment(s.db, orm.FilterEq("at_uri", commentAt))
	if err != nil {
		l.Error("failed to fetch comment", "aturi", commentAt)
		http.Error(w, "Failed to fetch comment", http.StatusInternalServerError)
		return
	}

	reactions, err := db.GetReactionMap(s.db, 20, comment.FeedCommentAtUri())
	if err != nil {
		l.Error("failed to get reactions", "err", err)
	}
	var userReactions map[models.ReactionKind]bool
	if user != nil {
		userReactions, err = db.GetReactionStatusMap(s.db, syntax.DID(user.Did), comment.FeedCommentAtUri())
		if err != nil {
			l.Error("failed to get user reactions", "err", err)
		}
	}

	err = s.pages.CommentBodyFragment(w, pages.CommentBodyFragmentParams{
		Comment:     comment,
		Reactions:   reactions,
		UserReacted: userReactions,
	})
	if err != nil {
		l.Error("failed to render")
	}
}

func (s *State) EditCommentFragment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "EditCommentFragment")

	commentAt := r.URL.Query().Get("aturi")
	comment, err := db.GetComment(s.db, orm.FilterEq("at_uri", commentAt))
	if err != nil {
		l.Error("failed to fetch comment", "aturi", commentAt)
		http.Error(w, "Failed to fetch comment", http.StatusInternalServerError)
		return
	}

	err = s.pages.EditCommentFragment(w, pages.EditCommentFragmentParams{
		Comment: comment,
	})
	if err != nil {
		l.Error("failed to render")
	}
}

func (s *State) NewReplyCommentFragment(w http.ResponseWriter, r *http.Request) {
	s.pages.ReplyCommentFragment(w, pages.ReplyCommentFragmentParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
	})
}

func (s *State) ReplyPlaceholderFragment(w http.ResponseWriter, r *http.Request) {
	s.pages.ReplyPlaceholderFragment(w, pages.ReplyPlaceholderFragmentParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
	})
}

func (s *State) NewComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "NewComment")
	user := s.oauth.GetMultiAccountUser(r)

	noticeId := "comment-error"
	ctx := r.Context()

	var pullRoundIdx *int
	if pullRoundIdxRaw := r.FormValue("pull-round-idx"); pullRoundIdxRaw != "" {
		roundIdx, err := strconv.Atoi(pullRoundIdxRaw)
		if err != nil {
			l.Warn("invalid round idx", "err", err)
			s.pages.Notice(w, noticeId, "pull round index should be valid integer")
			return
		}
		pullRoundIdx = &roundIdx
		noticeId = fmt.Sprintf("comment-error-%d", roundIdx)
	}

	body := r.FormValue("body")
	if body == "" {
		s.pages.Notice(w, noticeId, "Body is required")
		return
	}

	// TODO(boltless): normalize markdown body
	normalizedBody := body
	mentions, references := s.mentionsResolver.Resolve(ctx, body)

	markdownBody := tangled.MarkupMarkdown{
		Text:     normalizedBody,
		Original: &body,
		Blobs:    nil,
	}

	subjectUri, err := syntax.ParseATURI(r.FormValue("subject-uri"))
	if err != nil {
		l.Warn("invalid subject uri", "err", err)
		s.pages.Notice(w, noticeId, "Subject URI should be valid AT-URI")
		return
	}
	l = l.With("subject.uri", subjectUri)

	// ingest CID of subject record on-demand.
	// TODO(boltless): appview should ingest CID of all atproto records
	var subjectCid syntax.CID
	if subjectCidRaw := r.FormValue("subject-cid"); subjectCidRaw != "" {
		subjectCid, err = syntax.ParseCID(subjectCidRaw)
		if err != nil {
			l.Warn("invalid subject cid", "err", err)
			s.pages.Notice(w, noticeId, "Subject CID should be valid CID")
			return
		}
	} else {
		l.Debug("fetching subject record CID")
		subjectCid, err = func(uri syntax.ATURI) (syntax.CID, error) {
			ident, err := s.idResolver.ResolveIdent(ctx, uri.Authority().String())
			if err != nil {
				return "", err
			}

			xrpcc := indigoxrpc.Client{Host: ident.PDSEndpoint()}
			out, err := comatproto.RepoGetRecord(ctx, &xrpcc, "", uri.Collection().String(), ident.DID.String(), uri.RecordKey().String())
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
		}(subjectUri)
		if err != nil {
			l.Error("failed to backfill subject record", "err", err)
			s.pages.Notice(w, noticeId, "failed to backfill subject record")
			return
		}
	}
	l = l.With("subject.cid", subjectCid)

	subject := comatproto.RepoStrongRef{
		Uri: subjectUri.String(),
		Cid: subjectCid.String(),
	}

	var replyTo *comatproto.RepoStrongRef
	replyToUriRaw := r.FormValue("reply-to-uri")
	replyToCidRaw := r.FormValue("reply-to-cid")
	if replyToUriRaw != "" {
		replyToUri, err := syntax.ParseATURI(replyToUriRaw)
		if err != nil {
			s.pages.Notice(w, noticeId, "reply-to-uri should be valid AT-URI")
			return
		}
		// force replyTo.uri to `sh.tangled.feed.comment` collection, even when they aren't.
		// we are expecting parent comment will be migrated later.
		replyToUri = syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", replyToUri.Authority(), tangled.FeedCommentNSID, replyToUri.RecordKey()))

		var replyToCid syntax.CID
		if replyToCidRaw != "" {
			replyToCid, err = syntax.ParseCID(replyToCidRaw)
			if err != nil {
				s.pages.Notice(w, noticeId, "reply-to-cid should be valid CID")
				return
			}
		} else {
			// guess parent comment cid
			subjectComment, err := db.GetComment(s.db, orm.FilterEq("did", replyToUri.Authority()), orm.FilterEq("rkey", replyToUri.RecordKey()))
			if err != nil {
				l.Warn("db: failed to query subject comment", "err", err)
				s.pages.Notice(w, noticeId, "Subject record is unknown.")
				return
			}
			if subjectComment.Deleted != nil {
				// leave cid empty. reply comment won't pass the schema validation.
			} else {
				// guess cid from content
				c, err := func() (cid.Cid, error) {
					buf := new(bytes.Buffer)
					if subjectComment.Subject.Cid == "" {
						subjectComment.Subject.Cid = subject.Cid
					}
					if err := subjectComment.AsRecord().MarshalCBOR(buf); err != nil {
						return cid.Undef, fmt.Errorf("MarshalCBOR: %w", err)
					}
					return cid.NewPrefixV1(cid.DagCBOR, multihash.SHA2_256).Sum(buf.Bytes())
				}()
				if err != nil {
					l.Warn("cbor: failed to guess parent comment cid", "err", err)
					s.pages.Notice(w, noticeId, "Parent comment is invalid.")
					return
				}
				replyToCid = syntax.CID(c.String())
			}
		}
		replyTo = &comatproto.RepoStrongRef{
			Uri: replyToUri.String(),
			Cid: replyToCid.String(),
		}
	}

	comment := models.Comment{
		Did:        syntax.DID(user.Did),
		Collection: tangled.FeedCommentNSID,
		Rkey:       syntax.RecordKey(tid.TID()),

		Subject:      subject,
		Body:         markdownBody,
		Created:      time.Now(),
		ReplyTo:      replyTo,
		PullRoundIdx: pullRoundIdx,
	}
	if err = comment.Validate(); err != nil {
		l.Error("failed to validate comment", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment.")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment.")
		return
	}

	// create a record first
	out, err := comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Collection: comment.Collection.String(),
		Repo:       comment.Did.String(),
		Rkey:       comment.Rkey.String(),
		Record:     &lexutil.LexiconTypeDecoder{Val: comment.AsRecord()},
	})
	if err != nil {
		l.Error("failed to create comment", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment.")
		return
	}

	comment.Cid = syntax.CID(out.Cid)

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment, try again later.")
		return
	}
	defer tx.Rollback()

	_, err = db.PutComment(tx, &comment, references)
	if err != nil {
		l.Error("failed to create comment", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment.")
		return
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment, try again later.")
		return
	}

	s.notifier.NewComment(ctx, &comment, mentions)

	if pullRoundIdx != nil {
		var buf bytes.Buffer
		if err := s.pages.PullCommentFragment(&buf, pages.PullCommentFragmentParams{
			LoggedInUser: user,
			Comment:      comment,
		}); err != nil {
			l.Error("failed to render pull comment fragment", "err", err)
			s.pages.HxRefresh(w)
			return
		}
		w.Header().Set("Content-Type", "text/html")
		w.Write(buf.Bytes())
		return
	}

	target, err := s.pages.MakeCommentUrl(ctx, comment.AtUri())
	if err != nil {
		s.pages.HxRefresh(w)
	}

	s.pages.HxLocation(w, target)
}

func (s *State) EditComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "EditComment")
	user := s.oauth.GetMultiAccountUser(r)

	noticeId := "comment-error"
	ctx := r.Context()

	commentAt := r.FormValue("aturi")
	comment, err := db.GetComment(s.db, orm.FilterEq("at_uri", commentAt))
	if err != nil {
		l.Error("failed to fetch comment", "aturi", commentAt, "err", err)
		s.pages.Notice(w, noticeId, "Failed to fetch comment")
		return
	}

	if comment.Did.String() != user.Did {
		l.Error("unauthorized comment edit", "expectedDid", comment.Did, "gotDid", user.Did)
		s.pages.Notice(w, noticeId, "You are not the author of this comment")
		return
	}

	body := r.FormValue("body")
	if body == "" {
		s.pages.Notice(w, noticeId, "Body is required")
		return
	}

	// TODO(boltless): normalize markdown body
	normalizedBody := body
	_, references := s.mentionsResolver.Resolve(ctx, body)

	now := time.Now()
	newComment := comment
	newComment.Body = tangled.MarkupMarkdown{
		Text:     normalizedBody,
		Original: &body,
		Blobs:    nil,
	}
	newComment.Edited = &now
	if err := newComment.Validate(); err != nil {
		l.Error("failed to validate comment", "err", err)
		s.pages.Notice(w, noticeId, "Failed to update comment.")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		s.pages.Notice(w, noticeId, "Failed to create comment. try again later.")
		return
	}

	// update the record first
	exCid := comment.Cid.String()
	out, err := comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Collection: newComment.Collection.String(),
		Repo:       newComment.Did.String(),
		Rkey:       newComment.Rkey.String(),
		SwapRecord: &exCid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: newComment.AsRecord(),
		},
	})
	if err != nil {
		l.Error("failed to update comment", "err", err)
		s.pages.Notice(w, noticeId, "Failed to update comment, try again later.")
		return
	}

	newComment.Cid = syntax.CID(out.Cid)

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, noticeId, "Failed to update comment, try again later.")
		return
	}
	defer tx.Rollback()

	_, err = db.PutComment(tx, &newComment, references)
	if err != nil {
		l.Error("failed to perform update-description query", "err", err)
		s.pages.Notice(w, noticeId, "Failed to update comment, try again later.")
		return
	}
	err = tx.Commit()
	if err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, noticeId, "Failed to update comment, try again later.")
		return
	}

	reactions, err := db.GetReactionMap(s.db, 20, comment.FeedCommentAtUri())
	if err != nil {
		l.Error("failed to get reactions", "err", err)
	}
	userReactions, err := db.GetReactionStatusMap(s.db, syntax.DID(user.Did), comment.FeedCommentAtUri())
	if err != nil {
		l.Error("failed to get user reactions", "err", err)
	}

	// TODO: return full comment fragment so we can update comment header too
	s.pages.CommentBodyFragment(w, pages.CommentBodyFragmentParams{
		Comment:     newComment,
		Reactions:   reactions,
		UserReacted: userReactions,
	})
}

func (s *State) DeleteComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "DeleteComment")
	user := s.oauth.GetMultiAccountUser(r)

	noticeId := "comment"
	ctx := r.Context()

	commentAt := r.URL.Query().Get("aturi")
	comment, err := db.GetComment(s.db, orm.FilterEq("at_uri", commentAt))
	if err != nil {
		l.Error("failed to fetch comment", "aturi", commentAt)
		s.pages.Notice(w, noticeId, "Failed to fetch comment.")
		return
	}

	if comment.Did.String() != user.Did {
		l.Error("unauthorized action", "expectedDid", comment.Did, "gotDid", user.Did)
		s.pages.Notice(w, noticeId, "you are not the author of this comment")
		return
	}

	if comment.Deleted != nil {
		s.pages.Notice(w, noticeId, "Comment already deleted")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		s.pages.Notice(w, "comment", "Failed to delete comment.")
		return
	}
	_, err = comatproto.RepoDeleteRecord(ctx, client, &comatproto.RepoDeleteRecord_Input{
		Collection: comment.Collection.String(),
		Repo:       comment.Did.String(),
		Rkey:       comment.Rkey.String(),
	})
	if err != nil {
		l.Error("failed to delete from PDS", "err", err)
		s.pages.Notice(w, noticeId, "Failed to delete comment, try again later.")
		return
	}

	// optimistic update for htmx response
	now := time.Now()
	comment.Body = tangled.MarkupMarkdown{}
	comment.Deleted = &now

	s.pages.CommentBodyFragment(w, pages.CommentBodyFragmentParams{
		Comment: comment,
	})
	s.pages.CommentHeaderFragment(w, pages.CommentHeaderFragmentParams{
		Comment:   comment,
		HxSwapOob: true,
	})
}
