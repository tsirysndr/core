package state

import (
	"net/http"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/tid"
)

func (s *State) Follow(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "Follow")
	currentUser := s.oauth.GetMultiAccountUser(r)

	subject := r.URL.Query().Get("subject")
	if subject == "" {
		l.Warn("invalid form")
		return
	}

	subjectIdent, err := s.idResolver.ResolveIdent(r.Context(), subject)
	if err != nil {
		l.Error("failed to follow, invalid did", "subject", subject, "err", err)
		return
	}

	if currentUser.Did == subjectIdent.DID.String() {
		l.Warn("cant follow or unfollow yourself")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		return
	}

	switch r.Method {
	case http.MethodPost:
		follow := models.Follow{
			UserDid:    currentUser.Did,
			SubjectDid: subjectIdent.DID.String(),
			Rkey:       tid.TID(),
			FollowedAt: time.Now(),
		}

		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			s.logger.Error("failed to start transaction", "err", err)
			return
		}
		defer tx.Rollback()

		if err := db.UpsertFollow(tx, follow); err != nil {
			s.logger.Error("failed to follow", "err", err)
			return
		}

		record := follow.AsRecord()
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.GraphFollowNSID,
			Repo:       currentUser.Did,
			Rkey:       follow.Rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &record,
			},
		})
		if err != nil {
			l.Error("failed to create atproto record", "err", err)
			return
		}

		l.Info("created atproto record", "uri", resp.Uri)

		if err := tx.Commit(); err != nil {
			s.logger.Error("failed to commit transaction", "err", err)
			// DB op failed but record is created in PDS. Ingester will backfill the missed operation
		}

		s.notifier.NewFollow(r.Context(), &follow)

		followStats, err := db.GetFollowerFollowingCount(s.db, subjectIdent.DID.String())
		if err != nil {
			l.Error("failed to get follow stats", "err", err)
		}

		s.pages.FollowFragment(w, pages.FollowFragmentParams{
			UserDid:        subjectIdent.DID.String(),
			FollowStatus:   models.IsFollowing,
			FollowersCount: followStats.Followers,
		})

		return
	case http.MethodDelete:
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
			return
		}
		defer tx.Rollback()

		follows, err := db.DeleteFollow(tx, syntax.DID(currentUser.Did), subjectIdent.DID)
		if err != nil {
			l.Error("failed to delete follows from db", "err", err)
			return
		}

		var writes []*comatproto.RepoApplyWrites_Input_Writes_Elem
		for _, followAt := range follows {
			writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
				RepoApplyWrites_Delete: &comatproto.RepoApplyWrites_Delete{
					Collection: tangled.GraphFollowNSID,
					Rkey:       followAt.RecordKey().String(),
				},
			})
		}
		_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
			Repo:   currentUser.Did,
			Writes: writes,
		})
		if err != nil {
			l.Error("failed to delete follows from PDS", "err", err)
			return
		}

		if err := tx.Commit(); err != nil {
			l.Error("failed to commit transaction", "err", err)
			// The record was deleted from the PDS but the local rollback kept it.
			// Ingester will backfill the missed operation
		}

		s.notifier.DeleteFollow(r.Context(), &models.Follow{
			UserDid:    currentUser.Did,
			SubjectDid: subjectIdent.DID.String(),
			// Rkey
			// FollowedAt
		})

		followStats, err := db.GetFollowerFollowingCount(s.db, subjectIdent.DID.String())
		if err != nil {
			l.Error("failed to get follow stats", "err", err)
		}

		s.pages.FollowFragment(w, pages.FollowFragmentParams{
			UserDid:        subjectIdent.DID.String(),
			FollowStatus:   models.IsNotFollowing,
			FollowersCount: followStats.Followers,
		})

		return
	}

}
