package pulls

import (
	"fmt"
	"net/http"
	"strconv"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/tid"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
)

func (s *Pulls) PullComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullComment")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	roundNumberStr := chi.URLParam(r, "round")
	roundNumber, err := strconv.Atoi(roundNumberStr)
	if err != nil || roundNumber >= len(pull.Submissions) {
		http.Error(w, "bad round id", http.StatusBadRequest)
		l.Error("failed to parse round id", "err", err, "round_number_str", roundNumberStr)
		return
	}

	switch r.Method {
	case http.MethodGet:
		s.pages.PullNewCommentFragment(w, pages.PullNewCommentParams{
			LoggedInUser: user,
			RepoInfo:     s.repoResolver.GetRepoInfo(r, user),
			Pull:         pull,
			RoundNumber:  roundNumber,
		})
		return
	case http.MethodPost:
		body := r.FormValue("body")
		if body == "" {
			s.pages.Notice(w, "pull-comment", "Comment body is required")
			return
		}

		// TODO(boltless): normalize markdown body
		normalizedBody := body
		mentions, references := s.mentionsResolver.Resolve(r.Context(), body)

		markdownBody := tangled.MarkupMarkdown{
			Text:     normalizedBody,
			Original: &body,
			Blobs:    nil,
		}

		// ingest CID of PR record on-demand.
		// TODO(boltless): appview should ingest CID of atproto records
		cid, err := func() (syntax.CID, error) {
			ident, err := s.idResolver.ResolveIdent(r.Context(), pull.OwnerDid)
			if err != nil {
				return "", err
			}

			xrpcc := indigoxrpc.Client{Host: ident.PDSEndpoint()}
			out, err := comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoPullNSID, pull.OwnerDid, pull.Rkey)
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
		}()
		if err != nil {
			s.logger.Error("failed to backfill subject PR record", "err", err)
			s.pages.Notice(w, "pull-comment", "failed to backfill subject record")
			return
		}
		pullStrongRef := comatproto.RepoStrongRef{
			Uri: pull.AtUri().String(),
			Cid: cid.String(),
		}

		comment := models.Comment{
			Did:        syntax.DID(user.Did),
			Collection: tangled.FeedCommentNSID,
			Rkey:       syntax.RecordKey(tid.TID()),

			Subject:      pullStrongRef,
			Body:         markdownBody,
			Created:      time.Now(),
			ReplyTo:      nil,
			PullRoundIdx: &roundNumber,
		}
		if err = comment.Validate(); err != nil {
			s.logger.Error("failed to validate comment", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		client, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			s.logger.Error("failed to get authorized client", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		out, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: comment.Collection.String(),
			Repo:       comment.Did.String(),
			Rkey:       comment.Rkey.String(),
			Record:     &lexutil.LexiconTypeDecoder{Val: comment.AsRecord()},
		})
		if err != nil {
			s.logger.Error("failed to create pull comment", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		comment.Cid = syntax.CID(out.Cid)

		// Start a transaction
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}
		defer tx.Rollback()

		// Create the pull comment in the database
		err = db.PutComment(tx, &comment, references)
		if err != nil {
			l.Error("failed to create pull comment in database", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		// Commit the transaction
		if err = tx.Commit(); err != nil {
			l.Error("failed to commit transaction", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		s.notifier.NewPullComment(r.Context(), &comment, mentions)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d#comment-%d", ownerSlashRepo, pull.PullId, comment.Id))
		return
	}
}
