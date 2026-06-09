package focus

import (
	"log/slog"
	"net/http"
	"strconv"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/idresolver"
)

type Focus struct {
	db       *db.DB
	oauth    *oauth.OAuth
	pages    *pages.Pages
	logger   *slog.Logger
	resolver *idresolver.Resolver
}

func New(database *db.DB, o *oauth.OAuth, res *idresolver.Resolver, p *pages.Pages, logger *slog.Logger) *Focus {
	return &Focus{
		db:       database,
		oauth:    o,
		pages:    p,
		resolver: res,
		logger:   logger,
	}
}

func (f *Focus) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()
	r.Use(middleware.AuthMiddleware(f.oauth))
	r.Post("/begin", f.BeginFocus)
	r.Post("/end", f.EndFocus)
	r.Post("/next", f.FocusNext)
	return r
}

// BeginFocus activates focus mode and redirects the user to the oldest unread
// focus-eligible notification.
func (f *Focus) BeginFocus(w http.ResponseWriter, r *http.Request) {
	l := f.logger.With("handler", "BeginFocus")
	did := f.oauth.GetDid(r)

	if err := db.BeginFocus(f.db, did); err != nil {
		l.Error("failed to begin focus", "err", err)
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}

	item, err := db.GetNextFocusItem(f.db, did)
	if err != nil {
		l.Error("failed to get first focus item", "err", err)
		_ = db.EndFocus(f.db, did)
		http.Redirect(w, r, "/notifications", http.StatusSeeOther)
		return
	}
	if item == nil {
		_ = db.EndFocus(f.db, did)
		http.Redirect(w, r, "/notifications", http.StatusSeeOther)
		return
	}

	target := item.URL(f.resolver)
	if target == "" {
		_ = db.EndFocus(f.db, did)
		http.Redirect(w, r, "/notifications", http.StatusSeeOther)
		return
	}

	http.Redirect(w, r, target, http.StatusSeeOther)
}

// EndFocus deactivates focus mode and redirects to /notifications.
func (f *Focus) EndFocus(w http.ResponseWriter, r *http.Request) {
	l := f.logger.With("handler", "EndFocus")
	did := f.oauth.GetDid(r)

	if err := db.EndFocus(f.db, did); err != nil {
		l.Error("failed to end focus", "err", err)
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}

	http.Redirect(w, r, "/notifications", http.StatusSeeOther)
}

// FocusNext marks the current focus notification as read and redirects to the
// next focus item. If the queue is empty, focus mode is ended.
func (f *Focus) FocusNext(w http.ResponseWriter, r *http.Request) {
	l := f.logger.With("handler", "FocusNext")
	did := f.oauth.GetDid(r)

	// Mark the current item read so it leaves the queue.
	if currentIDStr := r.FormValue("current_id"); currentIDStr != "" {
		if currentID, err := strconv.ParseInt(currentIDStr, 10, 64); err == nil && currentID > 0 {
			if err := db.MarkNotificationRead(f.db, currentID, did); err != nil {
				l.Warn("failed to mark notification read", "id", currentID, "err", err)
			}
		}
	}

	item, err := db.GetNextFocusItem(f.db, did)
	if err != nil {
		l.Error("failed to get next focus item", "err", err)
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}
	if item == nil {
		_ = db.EndFocus(f.db, did)
		http.Redirect(w, r, "/notifications", http.StatusSeeOther)
		return
	}

	target := item.URL(f.resolver)
	if target == "" {
		_ = db.EndFocus(f.db, did)
		http.Redirect(w, r, "/notifications", http.StatusSeeOther)
		return
	}

	http.Redirect(w, r, target, http.StatusSeeOther)
}
