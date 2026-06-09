package timeline

import (
	"net/http"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/pages"
)

func (t *Timeline) Home(w http.ResponseWriter, r *http.Request) {
	// TODO: set this flag based on the UI
	filtered := false

	user := t.oauth.GetMultiAccountUser(r)

	timeline, err := db.MakeTimeline(t.db, 50, "", filtered)
	if err != nil {
		t.logger.Error("failed to make timeline", "err", err)
		t.pages.Notice(w, "timeline", "Uh oh! Failed to load timeline.")
		return
	}

	blueskyPosts, err := db.GetBlueskyPosts(t.db, 8)
	if err != nil {
		t.logger.Error("failed to get bluesky posts", "err", err)
	}

	t.pages.Home(w, pages.TimelineParams{
		BaseParams:      pages.BaseParamsFromContext(r.Context()),
		Timeline:        timeline,
		BlueskyPosts:    blueskyPosts,
		RecentBlogPosts: t.recentPosts,
		ShowNewsletter:  t.showNewsletter(user),
	})
}

func (t *Timeline) HomeOrTimeline(w http.ResponseWriter, r *http.Request) {
	if t.oauth.GetMultiAccountUser(r) != nil {
		t.Timeline(w, r)
		return
	}
	t.Home(w, r)
}
