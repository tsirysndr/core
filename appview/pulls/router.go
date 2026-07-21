package pulls

import (
	"net/http"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/pipelines"
)

func (s *Pulls) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()
	r.With(middleware.Paginate).Get("/", s.RepoPulls)
	r.Get("/pipeline-statuses", pipelines.StatusesHandler(s.oauth, s.repoResolver, s.pages, s.logger))
	r.With(middleware.AuthMiddleware(s.oauth)).Route("/new", func(r chi.Router) {
		r.Get("/", s.NewPull)
		r.Get("/refresh", s.RefreshCompose)
		r.Post("/refresh", s.RefreshCompose)
		r.Post("/", s.NewPull)
	})

	r.Route("/{pull}", func(r chi.Router) {
		r.Use(mw.ResolvePull())
		r.Get("/", s.RepoSinglePull)
		r.Get("/opengraph", s.PullOpenGraphSummary)

		r.Route("/round/{round}", func(r chi.Router) {
			r.Get("/", s.RepoPullPatch)
			r.Get("/interdiff", s.RepoPullInterdiff)
			r.Get("/actions", s.PullActions)
		})

		r.Route("/round/{round}.patch", func(r chi.Router) {
			r.Get("/", s.RepoPullPatchRaw)
		})

		r.Group(func(r chi.Router) {
			r.Use(middleware.AuthMiddleware(s.oauth))
			r.Get("/edit", s.EditPull)
			r.Post("/edit", s.EditPull)
			r.Route("/resubmit", func(r chi.Router) {
				r.Get("/", s.ResubmitPull)
				r.Post("/", s.ResubmitPull)
			})
			// permissions here require us to know pull author
			// it is handled within the route
			r.Post("/close", s.ClosePull)
			r.Post("/reopen", s.ReopenPull)
			r.Post("/subscribe", s.SubscribePull)
			// collaborators only
			r.Group(func(r chi.Router) {
				r.Use(mw.RepoPermissionMiddleware("repo:push"))
				r.Post("/merge", s.MergePull)
				// maybe lock, etc.
			})

			r.Group(func(r chi.Router) {
				r.Use(mw.RepoPermissionMiddleware("repo:push"))
				r.Post("/trigger-ci", s.TriggerCi)
			})
		})
	})
	return r

}
