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

func (s *State) React(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "React")
	currentUser := s.oauth.GetMultiAccountUser(r)

	subject := r.FormValue("subject-uri")
	if subject == "" {
		l.Warn("invalid form")
		return
	}

	subjectUri, err := syntax.ParseATURI(subject)
	if err != nil {
		l.Warn("invalid form", "subject", subject, "err", err)
		return
	}

	// override collection NSID to new one
	subjectUri = models.NormalizeReactionSubject(subjectUri)

	reactionKind, ok := models.ParseReactionKind(r.URL.Query().Get("kind"))
	if !ok {
		l.Warn("invalid reaction kind", "kind", r.URL.Query().Get("kind"))
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		return
	}

	switch r.Method {
	case http.MethodPost:
		createdAt := time.Now()
		rkey := tid.TID()
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.FeedReactionNSID,
			Repo:       currentUser.Did,
			Rkey:       rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.FeedReaction{
					Subject:   subjectUri.String(),
					Reaction:  reactionKind.String(),
					CreatedAt: createdAt.Format(time.RFC3339),
				},
			},
		})
		if err != nil {
			l.Error("failed to create atproto record", "err", err)
			return
		}

		err = db.AddReaction(s.db, currentUser.Did, subjectUri, reactionKind, rkey, createdAt)
		if err != nil {
			l.Error("failed to react", "err", err)
			return
		}

		reactionMap, err := db.GetReactionMap(s.db, 20, subjectUri)
		if err != nil {
			l.Error("failed to get reactions", "subjectUri", subjectUri, "err", err)
		}

		l.Info("created atproto record", "uri", resp.Uri)

		s.pages.ThreadReactionFragment(w, pages.ThreadReactionFragmentParams{
			Kind:        reactionKind,
			Count:       reactionMap[reactionKind].Count,
			Users:       reactionMap[reactionKind].Users,
			IsReacted:   true,
			CommentRkey: subjectUri.RecordKey().String(),
			SubjectUri:  subject,
		})

		return
	case http.MethodDelete:
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
		}
		defer tx.Rollback()

		reactions, err := db.DeleteReaction(tx, syntax.DID(currentUser.Did), subjectUri, reactionKind)
		if err != nil {
			l.Error("failed to delete reactions from db", "err", err)
			return
		}

		var writes []*comatproto.RepoApplyWrites_Input_Writes_Elem
		for _, reactionAt := range reactions {
			writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
				RepoApplyWrites_Delete: &comatproto.RepoApplyWrites_Delete{
					Collection: tangled.FeedReactionNSID,
					Rkey:       reactionAt.RecordKey().String(),
				},
			})
		}
		_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
			Repo:   currentUser.Did,
			Writes: writes,
		})
		if err != nil {
			l.Error("failed to delete reactions from PDS", "err", err)
			return
		}

		if err := tx.Commit(); err != nil {
			l.Error("failed to commit transaction", "err", err)
			// DB op failed but record is created in PDS. Ingester will backfill the missed operation
		}

		reactionMap, err := db.GetReactionMap(s.db, 20, subjectUri)
		if err != nil {
			l.Error("failed to get reactions", "subjectUri", subjectUri, "err", err)
			return
		}

		s.pages.ThreadReactionFragment(w, pages.ThreadReactionFragmentParams{
			Kind:        reactionKind,
			Count:       reactionMap[reactionKind].Count,
			Users:       reactionMap[reactionKind].Users,
			IsReacted:   false,
			CommentRkey: subjectUri.RecordKey().String(),
			SubjectUri:  subject,
		})

		return
	}
}
