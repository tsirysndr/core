package notifications

import (
	"log/slog"
	"net/http"
	"strconv"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

type Notifications struct {
	db     *db.DB
	oauth  *oauth.OAuth
	pages  *pages.Pages
	logger *slog.Logger
}

func New(database *db.DB, oauthHandler *oauth.OAuth, pagesHandler *pages.Pages, logger *slog.Logger) *Notifications {
	return &Notifications{
		db:     database,
		oauth:  oauthHandler,
		pages:  pagesHandler,
		logger: logger,
	}
}

func (n *Notifications) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()

	r.Get("/count", n.getUnreadCount)

	r.Group(func(r chi.Router) {
		r.Use(middleware.AuthMiddleware(n.oauth))
		r.With(middleware.Paginate).Get("/", n.notificationsPage)
		r.Get("/preview", n.previewHandler)
		r.Post("/{id}/read", n.markRead)
		r.Post("/read-all", n.markAllRead)
		r.Delete("/{id}", n.deleteNotification)
	})

	return r
}

func notificationFilters(r *http.Request, userDid string) (filters []orm.Filter, readFilter, categoryFilter string) {
	filters = []orm.Filter{orm.FilterEq("recipient_did", userDid)}

	readFilter = r.URL.Query().Get("read")
	if readFilter != "unread" {
		readFilter = "inbox"
	}
	if readFilter == "unread" {
		filters = append(filters, orm.FilterEq("read", 0))
	}

	categoryFilter = r.URL.Query().Get("category")
	switch categoryFilter {
	case "social":
		filters = append(filters, orm.FilterIn("type", models.SocialNotificationTypes))
	case "work":
		filters = append(filters, orm.FilterIn("type", models.WorkNotificationTypes))
	default:
		categoryFilter = "all"
	}

	return filters, readFilter, categoryFilter
}

func (n *Notifications) notificationsPage(w http.ResponseWriter, r *http.Request) {
	l := n.logger.With("handler", "notificationsPage")
	user := n.oauth.GetMultiAccountUser(r)

	page := pagination.FromContext(r.Context())
	filters, readFilter, categoryFilter := notificationFilters(r, user.Did)

	// mobile: respects category filter
	mobileTotal, err := db.CountNotifications(n.db, filters...)
	if err != nil {
		l.Error("failed to get total notifications", "err", err)
		n.pages.Error500(w)
		return
	}
	notifications, err := db.GetNotificationsWithEntities(n.db, page, filters...)
	if err != nil {
		l.Error("failed to get notifications", "err", err)
		n.pages.Error500(w)
		return
	}

	// desktop columns: category is fixed, only read filter applies
	readFilters := []orm.Filter{orm.FilterEq("recipient_did", user.Did)}
	if readFilter == "unread" {
		readFilters = append(readFilters, orm.FilterEq("read", 0))
	}
	workTotal, err := db.CountNotifications(n.db,
		append(readFilters, orm.FilterIn("type", models.WorkNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to count work notifications", "err", err)
		n.pages.Error500(w)
		return
	}
	workNotifications, err := db.GetNotificationsWithEntities(n.db, page,
		append(readFilters, orm.FilterIn("type", models.WorkNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to get work notifications", "err", err)
		n.pages.Error500(w)
		return
	}
	socialTotal, err := db.CountNotifications(n.db,
		append(readFilters, orm.FilterIn("type", models.SocialNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to count social notifications", "err", err)
		n.pages.Error500(w)
		return
	}
	socialNotifications, err := db.GetNotificationsWithEntities(n.db, page,
		append(readFilters, orm.FilterIn("type", models.SocialNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to get social notifications", "err", err)
		n.pages.Error500(w)
		return
	}

	// shared pagination total: max of all relevant counts
	total := int(max(socialTotal, max(workTotal, mobileTotal)))

	unreadBase := []orm.Filter{
		orm.FilterEq("recipient_did", user.Did),
		orm.FilterEq("read", 0),
	}
	workUnreadCount, err := db.CountNotifications(n.db,
		append(unreadBase, orm.FilterIn("type", models.WorkNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to count work unread", "err", err)
	}
	socialUnreadCount, err := db.CountNotifications(n.db,
		append(unreadBase, orm.FilterIn("type", models.SocialNotificationTypes))...,
	)
	if err != nil {
		l.Error("failed to count social unread", "err", err)
	}

	err = n.pages.Notifications(w, pages.NotificationsParams{
		LoggedInUser:      user,
		MobileGroups:      pages.GroupNotificationsByDate(notifications),
		WorkGroups:        pages.GroupNotificationsByDate(workNotifications),
		SocialGroups:      pages.GroupNotificationsByDate(socialNotifications),
		WorkUnreadCount:   workUnreadCount,
		SocialUnreadCount: socialUnreadCount,
		Page:              page,
		Total:             total,
		ReadFilter:        readFilter,
		CategoryFilter:    categoryFilter,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
}

func (n *Notifications) previewHandler(w http.ResponseWriter, r *http.Request) {
	l := n.logger.With("handler", "previewHandler")
	user := n.oauth.GetMultiAccountUser(r)

	filters, readFilter, categoryFilter := notificationFilters(r, user.Did)

	notifications, err := db.GetNotificationsWithEntities(
		n.db,
		pagination.Page{Limit: 5, Offset: 0},
		filters...,
	)
	if err != nil {
		l.Error("failed to get notifications", "err", err)
		n.pages.Error500(w)
		return
	}

	err = n.pages.NotificationPreview(w, pages.NotificationPreviewParams{
		LoggedInUser:   user,
		Notifications:  notifications,
		ReadFilter:     readFilter,
		CategoryFilter: categoryFilter,
	})
	if err != nil {
		l.Error("failed to render notification preview", "err", err)
	}
}

func (n *Notifications) getUnreadCount(w http.ResponseWriter, r *http.Request) {
	user := n.oauth.GetMultiAccountUser(r)
	if user == nil {
		http.Error(w, "Forbidden", http.StatusUnauthorized)
		return
	}

	count, err := db.CountNotifications(
		n.db,
		orm.FilterEq("recipient_did", user.Did),
		orm.FilterEq("read", 0),
	)
	if err != nil {
		http.Error(w, "Failed to get unread count", http.StatusInternalServerError)
		return
	}

	params := pages.NotificationCountParams{
		Count: count,
	}
	err = n.pages.NotificationCount(w, params)
	if err != nil {
		http.Error(w, "Failed to render count", http.StatusInternalServerError)
		return
	}
}

func (n *Notifications) markRead(w http.ResponseWriter, r *http.Request) {
	userDid := n.oauth.GetDid(r)

	idStr := chi.URLParam(r, "id")
	notificationID, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		http.Error(w, "Invalid notification ID", http.StatusBadRequest)
		return
	}

	err = db.MarkNotificationRead(n.db, notificationID, userDid)
	if err != nil {
		http.Error(w, "Failed to mark notification as read", http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusNoContent)
}

func (n *Notifications) markAllRead(w http.ResponseWriter, r *http.Request) {
	userDid := n.oauth.GetDid(r)

	err := db.MarkAllNotificationsRead(n.db, userDid)
	if err != nil {
		http.Error(w, "Failed to mark all notifications as read", http.StatusInternalServerError)
		return
	}

	http.Redirect(w, r, "/notifications", http.StatusSeeOther)
}

func (n *Notifications) deleteNotification(w http.ResponseWriter, r *http.Request) {
	userDid := n.oauth.GetDid(r)

	idStr := chi.URLParam(r, "id")
	notificationID, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		http.Error(w, "Invalid notification ID", http.StatusBadRequest)
		return
	}

	err = db.DeleteNotification(n.db, notificationID, userDid)
	if err != nil {
		http.Error(w, "Failed to delete notification", http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}
