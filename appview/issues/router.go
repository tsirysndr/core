package issues

import (
	"net/http"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/middleware"
)

func (i *Issues) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()

	r.Route("/", func(r chi.Router) {
		r.With(middleware.Paginate).Get("/", i.RepoIssues)

		r.Route("/{issue}", func(r chi.Router) {
			r.Use(mw.ResolveIssue)
			r.Get("/", i.RepoSingleIssue)
			r.Get("/opengraph", i.IssueOpenGraphSummary)

			// authenticated routes
			r.Group(func(r chi.Router) {
				r.Use(middleware.AuthMiddleware(i.oauth))
				r.Get("/edit", i.EditIssue)
				r.Post("/edit", i.EditIssue)
				r.Delete("/", i.DeleteIssue)
				r.Post("/close", i.CloseIssue)
				r.Post("/reopen", i.ReopenIssue)
			})
		})

		r.Group(func(r chi.Router) {
			r.Use(middleware.AuthMiddleware(i.oauth))
			r.Get("/new", i.NewIssue)
			r.Post("/new", i.NewIssue)
		})
	})

	return r
}
