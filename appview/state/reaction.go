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
		createdAt := time.Now().Format(time.RFC3339)
		rkey := tid.TID()
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.FeedReactionNSID,
			Repo:       currentUser.Did,
			Rkey:       rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.FeedReaction{
					Subject:   subjectUri.String(),
					Reaction:  reactionKind.String(),
					CreatedAt: createdAt,
				},
			},
		})
		if err != nil {
			l.Error("failed to create atproto record", "err", err)
			return
		}

		err = db.AddReaction(s.db, currentUser.Did, subjectUri, reactionKind, rkey)
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
			Kind:      reactionKind,
			Count:     reactionMap[reactionKind].Count,
			Users:     reactionMap[reactionKind].Users,
			IsReacted: true,
		})

		return
	case http.MethodDelete:
		reaction, err := db.GetReaction(s.db, currentUser.Did, subjectUri, reactionKind)
		if err != nil {
			l.Error("failed to get reaction relationship", "did", currentUser.Did, "subjectUri", subjectUri, "err", err)
			return
		}

		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.FeedReactionNSID,
			Repo:       currentUser.Did,
			Rkey:       reaction.Rkey,
		})

		if err != nil {
			l.Error("failed to remove reaction", "err", err)
			return
		}

		err = db.DeleteReactionByRkey(s.db, currentUser.Did, reaction.Rkey)
		if err != nil {
			l.Warn("failed to delete reaction from DB", "err", err)
			// this is not an issue, the firehose event might have already done this
		}

		reactionMap, err := db.GetReactionMap(s.db, 20, subjectUri)
		if err != nil {
			l.Error("failed to get reactions", "subjectUri", subjectUri, "err", err)
			return
		}

		s.pages.ThreadReactionFragment(w, pages.ThreadReactionFragmentParams{
			Kind:      reactionKind,
			Count:     reactionMap[reactionKind].Count,
			Users:     reactionMap[reactionKind].Users,
			IsReacted: false,
		})

		return
	}
}
