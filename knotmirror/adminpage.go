package knotmirror

import (
	"database/sql"
	"embed"
	"encoding/json"
	"fmt"
	"html"
	"html/template"
	"log/slog"
	"net/http"
	"strconv"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/knotmirror/xrpc"
)

//go:embed templates/*.html
var templateFS embed.FS

const repoPageSize = 20

type AdminServer struct {
	db       *sql.DB
	resyncer *Resyncer
	xrpc     *xrpc.Xrpc
	logger   *slog.Logger
}

func NewAdminServer(l *slog.Logger, database *sql.DB, resyncer *Resyncer, x *xrpc.Xrpc) *AdminServer {
	return &AdminServer{
		db:       database,
		resyncer: resyncer,
		xrpc:     x,
		logger:   l,
	}
}

func (s *AdminServer) Router() http.Handler {
	r := chi.NewRouter()
	r.Get("/repos", s.handleRepos())
	r.Get("/hosts", s.handleHosts())

	r.Post("/api/triggerRepoResync", s.handleRepoResyncTrigger())
	r.Post("/api/cancelRepoResync", s.handleRepoResyncCancel())
	r.Get("/api/inflight", s.handleInflight())
	return r
}

func (s *AdminServer) handleInflight() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		entries := s.xrpc.Inflight()
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(entries)
	}
}

func funcmap() template.FuncMap {
	return template.FuncMap{
		"add": func(a, b int) int { return a + b },
		"sub": func(a, b int) int { return a - b },
		"readt": func(ts int64) string {
			if ts <= 0 {
				return "n/a"
			}
			return time.Unix(ts, 0).Format("2006-01-02 15:04")
		},
		"const": func() map[string]any {
			return map[string]any{
				"AllRepoStates":   models.AllRepoStates,
				"AllHostStatuses": models.AllHostStatuses,
			}
		},
	}
}

func (s *AdminServer) handleRepos() http.HandlerFunc {
	tpl := template.Must(template.New("").Funcs(funcmap()).ParseFS(templateFS, "templates/base.html", "templates/repos.html"))
	return func(w http.ResponseWriter, r *http.Request) {
		pageNum, _ := strconv.Atoi(r.URL.Query().Get("page"))
		if pageNum < 1 {
			pageNum = 1
		}
		page := pagination.Page{
			Offset: (pageNum - 1) * repoPageSize,
			Limit:  repoPageSize,
		}

		var (
			did   = r.URL.Query().Get("did")
			knot  = r.URL.Query().Get("knot")
			state = r.URL.Query().Get("state")
		)

		repos, err := db.ListRepos(r.Context(), s.db, page, did, knot, state)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		counts, err := db.GetRepoCountsByState(r.Context(), s.db)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		err = tpl.ExecuteTemplate(w, "base", map[string]any{
			"Repos":         repos,
			"RepoCounts":    counts,
			"Page":          pageNum,
			"FilterByDid":   did,
			"FilterByKnot":  knot,
			"FilterByState": models.RepoState(state),
		})
		if err != nil {
			slog.Error("failed to render", "err", err)
		}
	}
}

func (s *AdminServer) handleHosts() http.HandlerFunc {
	tpl := template.Must(template.New("").Funcs(funcmap()).ParseFS(templateFS, "templates/base.html", "templates/hosts.html"))
	return func(w http.ResponseWriter, r *http.Request) {
		var status = models.HostStatus(r.URL.Query().Get("status"))
		if status == "" {
			status = models.HostStatusActive
		}

		hosts, err := db.ListHosts(r.Context(), s.db, status)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		err = tpl.ExecuteTemplate(w, "base", map[string]any{
			"Hosts":          hosts,
			"FilterByStatus": models.HostStatus(status),
		})
		if err != nil {
			slog.Error("failed to render", "err", err)
		}
	}
}

func (s *AdminServer) handleRepoResyncTrigger() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var repoQuery = r.FormValue("repo")

		repo, err := syntax.ParseATURI(repoQuery)
		if err != nil || repo.RecordKey() == "" {
			writeNotif(w, http.StatusBadRequest, fmt.Sprintf("repo parameter invalid: %s", repoQuery))
			return
		}

		if err := s.resyncer.TriggerResyncJob(r.Context(), repo); err != nil {
			s.logger.Error("failed to trigger resync job", "err", err)
			writeNotif(w, http.StatusInternalServerError, fmt.Sprintf("repo parameter invalid: %s", repoQuery))
			return
		}
		writeNotif(w, http.StatusOK, "success")
	}
}

func (s *AdminServer) handleRepoResyncCancel() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var repoQuery = r.FormValue("repo")

		repo, err := syntax.ParseATURI(repoQuery)
		if err != nil || repo.RecordKey() == "" {
			writeNotif(w, http.StatusBadRequest, fmt.Sprintf("repo parameter invalid: %s", repoQuery))
			return
		}

		s.resyncer.CancelResyncJob(repo)
		writeNotif(w, http.StatusOK, "success")
	}
}

func writeNotif(w http.ResponseWriter, status int, msg string) {
	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(status)

	class := "info"
	switch {
	case status >= 500:
		class = "error"
	case status >= 400:
		class = "warn"
	}

	fmt.Fprintf(w,
		`<div hx-swap-oob="beforeend:#notifications"><div class="notif %s">%s</div></div>`,
		class,
		html.EscapeString(msg),
	)
}
