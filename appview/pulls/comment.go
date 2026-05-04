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
	lexutil "github.com/bluesky-social/indigo/lex/util"
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
			s.pages.Notice(w, "pull", "Comment body is required")
			return
		}

		mentions, references := s.mentionsResolver.Resolve(r.Context(), body)

		// Start a transaction
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}
		defer tx.Rollback()

		createdAt := time.Now().Format(time.RFC3339)

		client, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}
		atResp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoPullCommentNSID,
			Repo:       user.Did,
			Rkey:       tid.TID(),
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.RepoPullComment{
					Pull:      pull.AtUri().String(),
					Body:      body,
					CreatedAt: createdAt,
				},
			},
		})
		if err != nil {
			l.Error("failed to create pull comment", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		comment := &models.PullComment{
			OwnerDid:     user.Did,
			RepoAt:       f.RepoAt().String(),
			PullId:       pull.PullId,
			Body:         body,
			CommentAt:    atResp.Uri,
			SubmissionId: pull.Submissions[roundNumber].ID,
			Mentions:     mentions,
			References:   references,
		}

		// Create the pull comment in the database with the commentAt field
		commentId, err := db.NewPullComment(tx, comment)
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

		s.notifier.NewPullComment(r.Context(), comment, mentions)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d#comment-%d", ownerSlashRepo, pull.PullId, commentId))
		return
	}
}
