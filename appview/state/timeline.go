package state

import (
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/orm"
)

func (s *State) Home(w http.ResponseWriter, r *http.Request) {
	// TODO: set this flag based on the UI
	filtered := false

	user := s.oauth.GetMultiAccountUser(r)

	timeline, err := db.MakeTimeline(s.db, 50, "", filtered)
	if err != nil {
		s.logger.Error("failed to make timeline", "err", err)
		s.pages.Notice(w, "timeline", "Uh oh! Failed to load timeline.")
		return
	}

	blueskyPosts, err := db.GetBlueskyPosts(s.db, 8)
	if err != nil {
		s.logger.Error("failed to get bluesky posts", "err", err)
	}

	s.pages.Home(w, pages.TimelineParams{
		LoggedInUser:   user,
		Timeline:       timeline,
		BlueskyPosts:   blueskyPosts,
		ShowNewsletter: s.showNewsletter(user),
	})
}
func (s *State) HomeOrTimeline(w http.ResponseWriter, r *http.Request) {
	if s.oauth.GetMultiAccountUser(r) != nil {
		s.Timeline(w, r)
		return
	}
	s.Home(w, r)
}

func (s *State) Timeline(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)

	// TODO: set this flag based on the UI
	filtered := false

	var userDid string
	if user != nil {
		userDid = user.Did
	}
	timeline, err := db.MakeTimeline(s.db, 50, userDid, filtered)
	if err != nil {
		s.logger.Error("failed to make timeline", "err", err)
		s.pages.Notice(w, "timeline", "Uh oh! Failed to load timeline.")
	}

	repos, err := db.GetTopStarredReposLastWeek(s.db)
	if err != nil {
		s.logger.Error("failed to get top starred repos", "err", err)
		s.pages.Notice(w, "topstarredrepos", "Unable to load.")
		return
	}

	gfiLabel, err := db.GetLabelDefinition(s.db, orm.FilterEq("at_uri", s.config.Label.GoodFirstIssue))
	if err != nil {
		// non-fatal
	}

	var vouchSuggestions []models.VouchSuggestion
	if user != nil {
		vouchSuggestions, err = db.GetVouchSuggestions(s.db, user.Did, 3)
		if err != nil {
			s.logger.Error("failed to get vouch suggestions", "err", err)
		}
		if len(vouchSuggestions) > 0 {
			suggestionDids := make([]syntax.DID, len(vouchSuggestions))
			for i, sv := range vouchSuggestions {
				suggestionDids[i] = syntax.DID(sv.Did)
			}
			relationships, err := db.GetVouchRelationshipsBatch(s.db, syntax.DID(user.Did), suggestionDids)
			if err != nil {
				s.logger.Error("failed to get vouch relationships for suggestions", "err", err)
			} else {
				for i := range vouchSuggestions {
					vouchSuggestions[i].VouchRelationship = relationships[vouchSuggestions[i].Did]
				}
			}
		}
	}

	s.pages.Timeline(w, pages.TimelineParams{
		LoggedInUser:     user,
		Timeline:         timeline,
		Repos:            repos,
		GfiLabel:         gfiLabel,
		VouchSuggestions: vouchSuggestions,
		ShowNewsletter:   s.showNewsletter(user),
	})
}

// showNewsletter decides whether the newsletter widget/CTA should render.
// Anonymous visitors always see it (they can dismiss via localStorage);
// logged-in users whose newsletter_preferences row exists (either
// subscribed or dismissed) do not.
func (s *State) showNewsletter(user *oauth.MultiAccountUser) bool {
	if user == nil {
		return true
	}
	pref, err := db.GetNewsletterPref(s.db, user.Did)
	if err != nil {
		s.logger.Error("failed to read newsletter preference", "did", user.Did, "err", err)
		return true
	}
	return pref == nil
}
