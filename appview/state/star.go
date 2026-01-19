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
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
		}
		defer tx.Rollback()

		stars, err := db.DeleteStars(tx, syntax.DID(currentUser.Did), subjectKey)
		if err != nil {
			l.Error("failed to delete stars from db", "err", err)
			return
		}

		var writes []*comatproto.RepoApplyWrites_Input_Writes_Elem
		for _, starAt := range stars {
			writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
				RepoApplyWrites_Delete: &comatproto.RepoApplyWrites_Delete{
					Collection: tangled.FeedStarNSID,
					Rkey:       starAt.RecordKey().String(),
				},
			})
		}
		_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
			Repo:   currentUser.Did,
			Writes: writes,
		})
		if err != nil {
			l.Error("failed to delete stars from PDS", "err", err)
			return
		}

		if err := tx.Commit(); err != nil {
			l.Error("failed to commit transaction", "err", err)
			// DB op failed but record is created in PDS. Ingester will backfill the missed operation
		}

		s.notifier.DeleteStar(r.Context(), &models.Star{
			Did:    currentUser.Did,
			SubjectType: subjectType,
			Subject: subjectKey,
			// Rkey
			// Created
		})

		starCount, err := db.GetStarCount(s.db, subjectType, subjectKey)
		if err != nil {
			l.Error("failed to get star count", "subject", subjectKey, "err", err)
			return
		}

		s.pages.StarBtnFragment(w, pages.StarBtnFragmentParams{
			IsStarred: false,
			SubjectAt: subjectUri,
			StarCount: starCount,
			RepoName:  repoName,
		})

		return
	}
}
