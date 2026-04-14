package state

import (
	"fmt"
	"log"
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

func resolveStarSubject(d db.Execer, subjectUri syntax.ATURI) (models.StarSubjectType, string, *tangled.FeedStar_Subject, error) {
	collection := subjectUri.Collection()

	switch collection.String() {
	case tangled.RepoNSID:
		repo, err := db.GetRepoByAtUri(d, subjectUri.String())
		if err != nil {
			return "", "", nil, err
		}
		if repo.RepoDid == "" {
			return "", "", nil, fmt.Errorf("repo has no DID: %s", subjectUri)
		}
		subject := &tangled.FeedStar_Subject{
			FeedStar_Repo: &tangled.FeedStar_Repo{Did: repo.RepoDid},
		}
		return models.StarSubjectRepo, repo.RepoDid, subject, nil

	case tangled.StringNSID:
		uri := subjectUri.String()
		subject := &tangled.FeedStar_Subject{
			FeedStar_String: &tangled.FeedStar_String{Uri: uri},
		}
		return models.StarSubjectString, uri, subject, nil

	default:
		return "", "", nil, fmt.Errorf("unsupported star subject collection: %s", collection)
	}
}

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

	subjectType, subjectKey, starSubject, err := resolveStarSubject(s.db, subjectUri)
	if err != nil {
		log.Println("failed to resolve star subject", err)
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

		starRecord := &tangled.FeedStar{
			CreatedAt: createdAt,
			Subject:   starSubject,
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
			Did:         currentUser.Did,
			SubjectType: subjectType,
			Subject:     subjectKey,
			Rkey:        rkey,
		}

		err = db.AddStar(s.db, star)
		if err != nil {
			l.Error("failed to star", "err", err)
			return
		}

		starCount, err := db.GetStarCount(s.db, subjectType, subjectKey)
		if err != nil {
			l.Error("failed to get star count", "subject", subjectKey, "err", err)
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
		star, err := db.GetStar(s.db, currentUser.Did, subjectKey)
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

		starCount, err := db.GetStarCount(s.db, subjectType, subjectKey)
		if err != nil {
			l.Error("failed to get star count", "subject", subjectKey, "err", err)
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
