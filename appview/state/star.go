package state

import (
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
	"tangled.org/core/orm"
	"tangled.org/core/tid"
)

func (s *State) Star(w http.ResponseWriter, r *http.Request) {
	currentUser := s.oauth.GetMultiAccountUser(r)

	subject := r.URL.Query().Get("subject")
	if subject == "" {
		log.Println("invalid form")
		return
	}

	subjectUri, err := syntax.ParseATURI(subject)
	if err != nil {
		log.Println("invalid form")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		log.Println("failed to authorize client", err)
		return
	}

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
			Repo:       currentUser.Active.Did,
			Rkey:       rkey,
			Record:     &lexutil.LexiconTypeDecoder{Val: starRecord},
		})
		if err != nil {
			log.Println("failed to create atproto record", err)
			return
		}
		log.Println("created atproto record: ", resp.Uri)

		star := &models.Star{
			Did:    currentUser.Active.Did,
			RepoAt: subjectUri,
			Rkey:   rkey,
		}

		err = db.AddStar(s.db, star)
		if err != nil {
			log.Println("failed to star", err)
			return
		}

		starCount, err := db.GetStarCount(s.db, subjectUri)
		if err != nil {
			log.Println("failed to get star count for ", subjectUri)
		}

		s.notifier.NewStar(r.Context(), star)

		s.pages.StarBtnFragment(w, pages.StarBtnFragmentParams{
			IsStarred: true,
			SubjectAt: subjectUri,
			StarCount: starCount,
		})

		return
	case http.MethodDelete:
		// find the record in the db
		star, err := db.GetStar(s.db, currentUser.Active.Did, subjectUri)
		if err != nil {
			log.Println("failed to get star relationship")
			return
		}

		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.FeedStarNSID,
			Repo:       currentUser.Active.Did,
			Rkey:       star.Rkey,
		})

		if err != nil {
			log.Println("failed to unstar")
			return
		}

		err = db.DeleteStarByRkey(s.db, currentUser.Active.Did, star.Rkey)
		if err != nil {
			log.Println("failed to delete star from DB")
			// this is not an issue, the firehose event might have already done this
		}

		starCount, err := db.GetStarCount(s.db, subjectUri)
		if err != nil {
			log.Println("failed to get star count for ", subjectUri)
			return
		}

		s.notifier.DeleteStar(r.Context(), star)

		s.pages.StarBtnFragment(w, pages.StarBtnFragmentParams{
			IsStarred: false,
			SubjectAt: subjectUri,
			StarCount: starCount,
		})

		return
	}

}
