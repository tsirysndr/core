package timeline

import (
	"net/http"
	"sort"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func (t *Timeline) Timeline(w http.ResponseWriter, r *http.Request) {
	user := t.oauth.GetMultiAccountUser(r)

	followingOnly := r.URL.Query().Get("following") == "true" && user != nil

	var userDid string
	if user != nil {
		userDid = user.Did
	}
	timeline, err := db.MakeTimeline(t.db, 50, userDid, followingOnly)
	if err != nil {
		t.logger.Error("failed to make timeline", "err", err)
		t.pages.Notice(w, "timeline", "Uh oh! Failed to load timeline.")
	}

	repos, err := db.GetTopStarredReposLastWeek(t.db)
	if err != nil {
		t.logger.Error("failed to get top starred repos", "err", err)
		t.pages.Notice(w, "topstarredrepos", "Unable to load.")
		return
	}

	gfiLabel, err := db.GetLabelDefinition(t.db, orm.FilterEq("at_uri", t.config.Label.GoodFirstIssue))
	if err != nil {
		// non-fatal
	}

	var notifications []*models.NotificationWithEntity
	if user != nil {
		notifications, err = db.GetNotificationsWithEntities(
			t.db,
			pagination.Page{Limit: 5, Offset: 0},
			orm.FilterEq("recipient_did", user.Did),
		)
		if err != nil {
			t.logger.Error("failed to get notifications for timeline", "err", err)
		}
	}

	var vouchSuggestions []models.VouchSuggestion
	if user != nil {
		vouchSuggestions, err = db.GetVouchSuggestions(t.db, user.Did, 3)
		if err != nil {
			t.logger.Error("failed to get vouch suggestions", "err", err)
		}
		if len(vouchSuggestions) > 0 {
			suggestionDids := make([]syntax.DID, len(vouchSuggestions))
			for i, sv := range vouchSuggestions {
				suggestionDids[i] = syntax.DID(sv.Did)
			}
			relationships, err := db.GetVouchRelationshipsBatch(t.db, syntax.DID(user.Did), suggestionDids)
			if err != nil {
				t.logger.Error("failed to get vouch relationships for suggestions", "err", err)
			} else {
				for i := range vouchSuggestions {
					vouchSuggestions[i].VouchRelationship = relationships[vouchSuggestions[i].Did]
				}
			}
		}
	}

	var recents []pages.RecentItem
	if user != nil {
		recents, err = t.buildRecents(user.Did)
		if err != nil {
			t.logger.Error("failed to build recents for timeline", "err", err)
		}
	}

	t.pages.Timeline(w, pages.TimelineParams{
		LoggedInUser:     user,
		Timeline:         timeline,
		Repos:            repos,
		GfiLabel:         gfiLabel,
		VouchSuggestions: vouchSuggestions,
		Notifications:    notifications,
		Recents:          recents,
		FollowingOnly:    followingOnly,
		RecentBlogPosts:  t.recentPosts,
		ShowNewsletter:   t.showNewsletter(user),
	})
}

func (t *Timeline) buildRecents(userDid string) ([]pages.RecentItem, error) {
	links, err := db.GetRecentLinks(t.db, orm.FilterEq("user_did", userDid))
	if err != nil {
		return nil, err
	}
	if len(links) == 0 {
		return nil, nil
	}

	// group targets by type.
	var repoDids, issueAtUris, pullAtUris []string
	for _, l := range links {
		switch l.LinkType {
		case models.RecentLinkTypeRepo:
			repoDids = append(repoDids, l.Target)
		case models.RecentLinkTypeIssue:
			issueAtUris = append(issueAtUris, l.Target)
		case models.RecentLinkTypePull:
			pullAtUris = append(pullAtUris, l.Target)
		}
	}

	// fetch repos by DID.
	repoByDid := make(map[string]*models.Repo)
	if len(repoDids) > 0 {
		fetched, err := db.GetRepos(t.db, orm.FilterIn("repo_did", repoDids))
		if err != nil {
			return nil, err
		}
		for i := range fetched {
			repoByDid[fetched[i].RepoDid] = &fetched[i]
		}
	}

	// fetch issues by aturi
	issueByAtUri := make(map[string]*models.Issue)
	if len(issueAtUris) > 0 {
		issues, err := db.GetIssues(t.db, orm.FilterIn("at_uri", issueAtUris))
		if err != nil {
			return nil, err
		}
		for _, issue := range issues {
			issueByAtUri[issue.AtUri().String()] = &issue
		}
	}

	// fetch pulls by aturi
	pullByAtUri := make(map[string]*models.Pull)
	if len(pullAtUris) > 0 {
		fetched, err := db.GetPulls(t.db, orm.FilterIn("at_uri", pullAtUris))
		if err != nil {
			return nil, err
		}
		for _, p := range fetched {
			pullByAtUri[p.AtUri().String()] = p
		}
	}

	// build result in original link order
	var items []pages.RecentItem
	for _, l := range links {
		item := pages.RecentItem{Link: l}
		switch l.LinkType {
		case models.RecentLinkTypeRepo:
			item.Repo = repoByDid[l.Target]
		case models.RecentLinkTypeIssue:
			item.Issue = issueByAtUri[l.Target]
		case models.RecentLinkTypePull:
			item.Pull = pullByAtUri[l.Target]
		}
		// skip if the entity could not be resolved (e.g. deleted).
		if item.Repo == nil && item.Issue == nil && item.Pull == nil {
			continue
		}
		items = append(items, item)
	}

	// re-sort by visited descending to restore recency order after map lookups.
	sort.Slice(items, func(i, j int) bool {
		return items[i].Link.Visited.After(items[j].Link.Visited)
	})

	return items, nil
}

// showNewsletter decides whether the newsletter widget/CTA should render.
// Anonymous visitors always see it (they can dismiss via localStorage);
// logged-in users whose newsletter_preferences row exists (either
// subscribed or dismissed) do not.
func (t *Timeline) showNewsletter(user *oauth.MultiAccountUser) bool {
	if user == nil {
		return true
	}
	pref, err := db.GetNewsletterPref(t.db, user.Did)
	if err != nil {
		t.logger.Error("failed to read newsletter preference", "did", user.Did, "err", err)
		return true
	}
	return pref == nil
}
