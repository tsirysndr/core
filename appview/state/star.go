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
	"tangled.org/core/orm"
	"tangled.org/core/tid"
)

func (s *State) Star(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "Star")
	currentUser := s.oauth.GetMultiAccountUser(r)

	subject := r.URL.Query().Get("subject")
	if subject == "" {
		l.Warn("invalid form")
		return
	}

	subjectUri, err := syntax.ParseATURI(subject)
	if err != nil {
		l.Warn("invalid form", "subject", subject, "err", err)
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		return
	}

	repoName := r.URL.Query().Get("repoName")

	switch r.Method {
	case http.MethodPost:
		createdAt := time.Now().Format(time.RFC3339)
		rkey := tid.TID()

		subjectStr := subjectUri.String()
		starRecord := &tangled.FeedStar{
			CreatedAt: createdAt,
			Subject:   &subjectStr,
		}
		repo, err := db.GetRepo(s.db, orm.FilterEq("at_uri", subjectUri.String()))
		if err == nil && repo.RepoDid != "" {
			starRecord.SubjectDid = &repo.RepoDid
		}

		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.FeedStarNSID,
			Repo:       currentUser.Did,
			Rkey:       rkey,
			Record:     &lexutil.LexiconTypeDecoder{Val: starRecord},
		})
		if err != nil {
			l.Error("failed to create atproto record", "err", err)
			return
		}
		l.Info("created atproto record", "uri", resp.Uri)

		star := &models.Star{
			Did:    currentUser.Did,
			RepoAt: subjectUri,
			Rkey:   rkey,
		}

		err = db.AddStar(s.db, star)
		if err != nil {
			l.Error("failed to star", "err", err)
			return
		}

		starCount, err := db.GetStarCount(s.db, subjectUri)
		if err != nil {
			l.Error("failed to get star count", "subjectUri", subjectUri, "err", err)
		}

		s.notifier.NewStar(r.Context(), star)

		s.pages.StarBtnFragment(w, pages.StarBtnFragmentParams{
			IsStarred: true,
			SubjectAt: subjectUri,
			StarCount: starCount,
			RepoName:  repoName,
		})

		return
	case http.MethodDelete:
		// find the record in the db
		star, err := db.GetStar(s.db, currentUser.Did, subjectUri)
		if err != nil {
			l.Error("failed to get star relationship", "err", err)
			return
		}

		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.FeedStarNSID,
			Repo:       currentUser.Did,
			Rkey:       star.Rkey,
		})

		if err != nil {
			l.Error("failed to unstar", "err", err)
			return
		}

		err = db.DeleteStarByRkey(s.db, currentUser.Did, star.Rkey)
		if err != nil {
			l.Warn("failed to delete star from DB", "err", err)
			// this is not an issue, the firehose event might have already done this
		}

		starCount, err := db.GetStarCount(s.db, subjectUri)
		if err != nil {
			l.Error("failed to get star count", "subjectUri", subjectUri, "err", err)
			return
		}

		s.notifier.DeleteStar(r.Context(), star)

		s.pages.StarBtnFragment(w, pages.StarBtnFragmentParams{
			IsStarred: false,
			SubjectAt: subjectUri,
			StarCount: starCount,
			RepoName:  repoName,
		})

		return
	}

}

