package xrpc

import (
	"encoding/json"
	"net/http"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	db "tangled.org/core/deliberi/db"
	"tangled.org/core/deliberi/models"
	"tangled.org/core/orm"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) NotificationList(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationList")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	q := r.URL.Query()
	filters := []orm.Filter{}
	if q.Get("read") == "unread" {
		filters = append(filters, orm.FilterEq("read", 0))
	}
	switch q.Get("category") {
	case "social":
		filters = append(filters, orm.FilterIn("type", models.SocialNotificationTypes))
	case "work":
		filters = append(filters, orm.FilterIn("type", models.WorkNotificationTypes))
	}

	limit := 50
	if s := q.Get("limit"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n > 0 && n <= 100 {
			limit = n
		}
	}

	notifs, err := db.GetNotifications(x.DB, did, limit, filters...)
	if err != nil {
		l.Error("failed to list notifications", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	unreadBase := []orm.Filter{orm.FilterEq("read", 0)}
	workUnread, _ := db.CountNotifications(x.DB, did, append(unreadBase, orm.FilterIn("type", models.WorkNotificationTypes))...)
	socialUnread, _ := db.CountNotifications(x.DB, did, append(unreadBase, orm.FilterIn("type", models.SocialNotificationTypes))...)

	items := make([]*tangled.TempNotificationListNotifications_Notification, 0, len(notifs))
	for _, n := range notifs {
		item := &tangled.TempNotificationListNotifications_Notification{
			Uri:       n.AtUri,
			Type:      string(n.Type),
			Category:  models.Category(n.Type),
			ActorDid:  n.ActorDid,
			Read:      n.Read,
			CreatedAt: n.Created.Format(timeFormat),
		}
		if n.RepoDid != "" {
			item.RepoDid = &n.RepoDid
		}
		if n.EntityAt != "" {
			switch syntax.ATURI(n.EntityAt).Collection().String() {
			case "sh.tangled.repo.issue":
				item.IssueAt = &n.EntityAt
			case "sh.tangled.repo.pull":
				item.PullAt = &n.EntityAt
			}
		}
		items = append(items, item)
	}

	x.writeJSON(w, &tangled.TempNotificationListNotifications_Output{
		Notifications:     items,
		SocialUnreadCount: socialUnread,
		WorkUnreadCount:   workUnread,
	})
}

func (x *Xrpc) NotificationGetUnreadCount(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationGetUnreadCount")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	count, err := db.CountNotifications(x.DB, did, orm.FilterEq("read", 0))
	if err != nil {
		l.Error("failed to count unread notifications", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	x.writeJSON(w, &tangled.TempNotificationGetUnreadCount_Output{Count: count})
}

func (x *Xrpc) NotificationUpdateSeen(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationUpdateSeen")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempNotificationUpdateSeen_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}
	if input.Uri == "" {
		writeError(w, badRequestError("uri is required"), http.StatusBadRequest)
		return
	}

	if err := db.MarkRead(x.DB, did, input.Uri, input.Read); err != nil {
		l.Error("failed to update notification read state", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) NotificationMarkAllRead(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationMarkAllRead")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	if err := db.MarkAllRead(x.DB, did); err != nil {
		l.Error("failed to mark all read", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) NotificationGetPreferences(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationGetPreferences")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	prefs, err := db.GetNotificationPreference(x.DB, did)
	if err != nil {
		l.Error("failed to get notification preferences", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	x.writeJSON(w, &tangled.TempNotificationGetPreferences_Preferences{
		EmailNotifications: prefs.EmailNotifications,
		Followed:           prefs.Followed,
		IssueClosed:        prefs.IssueClosed,
		IssueCommented:     prefs.IssueCommented,
		IssueCreated:       prefs.IssueCreated,
		PullCommented:      prefs.PullCommented,
		PullCreated:        prefs.PullCreated,
		PullMerged:         prefs.PullMerged,
		RepoStarred:        prefs.RepoStarred,
		UserMentioned:      prefs.UserMentioned,
	})
}

func (x *Xrpc) NotificationUpdatePreferences(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "NotificationUpdatePreferences")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempNotificationUpdatePreferences_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	existing, err := db.GetNotificationPreference(x.DB, did)
	if err != nil {
		l.Error("failed to get existing notification preferences", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	prefs := &models.NotificationPreferences{
		UserDid:            syntax.DID(did),
		RepoStarred:        applyBoolPtr(existing.RepoStarred, input.RepoStarred),
		IssueCreated:       applyBoolPtr(existing.IssueCreated, input.IssueCreated),
		IssueCommented:     applyBoolPtr(existing.IssueCommented, input.IssueCommented),
		IssueClosed:        applyBoolPtr(existing.IssueClosed, input.IssueClosed),
		PullCreated:        applyBoolPtr(existing.PullCreated, input.PullCreated),
		PullCommented:      applyBoolPtr(existing.PullCommented, input.PullCommented),
		PullMerged:         applyBoolPtr(existing.PullMerged, input.PullMerged),
		Followed:           applyBoolPtr(existing.Followed, input.Followed),
		UserMentioned:      applyBoolPtr(existing.UserMentioned, input.UserMentioned),
		EmailNotifications: applyBoolPtr(existing.EmailNotifications, input.EmailNotifications),
	}

	if err := db.UpsertNotificationPreferences(x.DB, prefs); err != nil {
		l.Error("failed to update notification preferences", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusOK)
}

func applyBoolPtr(existing bool, update *bool) bool {
	if update != nil {
		return *update
	}
	return existing
}
