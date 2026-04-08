package state

import (
	"net/http"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
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

	if currentUser.Active.Did == subjectIdent.DID.String() {
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
		createdAt := time.Now().Format(time.RFC3339)
		rkey := tid.TID()
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.GraphFollowNSID,
			Repo:       currentUser.Active.Did,
			Rkey:       rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.GraphFollow{
					Subject:   subjectIdent.DID.String(),
					CreatedAt: createdAt,
				}},
		})
		if err != nil {
			l.Error("failed to create atproto record", "err", err)
			return
		}

		l.Info("created atproto record", "uri", resp.Uri)

		follow := &models.Follow{
			UserDid:    currentUser.Active.Did,
			SubjectDid: subjectIdent.DID.String(),
			Rkey:       rkey,
		}

		err = db.AddFollow(s.db, follow)
		if err != nil {
			l.Error("failed to follow", "err", err)
			return
		}

		s.notifier.NewFollow(r.Context(), follow)

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
		// find the record in the db
		follow, err := db.GetFollow(s.db, currentUser.Active.Did, subjectIdent.DID.String())
		if err != nil {
			l.Error("failed to get follow relationship", "err", err)
			return
		}

		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.GraphFollowNSID,
			Repo:       currentUser.Active.Did,
			Rkey:       follow.Rkey,
		})

		if err != nil {
			l.Error("failed to unfollow", "err", err)
			return
		}

		err = db.DeleteFollowByRkey(s.db, currentUser.Active.Did, follow.Rkey)
		if err != nil {
			l.Warn("failed to delete follow from DB", "err", err)
			// this is not an issue, the firehose event might have already done this
		}

		followStats, err := db.GetFollowerFollowingCount(s.db, subjectIdent.DID.String())
		if err != nil {
			l.Error("failed to get follow stats", "err", err)
		}

		s.pages.FollowFragment(w, pages.FollowFragmentParams{
			UserDid:        subjectIdent.DID.String(),
			FollowStatus:   models.IsNotFollowing,
			FollowersCount: followStats.Followers,
		})

		s.notifier.DeleteFollow(r.Context(), follow)

		return
	}

}
