package state

import (
	"cmp"
	"net/http"
	"slices"
	"strings"
	"time"

	"github.com/posthog/posthog-go"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/searchquery"
	"tangled.org/core/orm"
)

func (s *State) Search(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "Search")

	params := r.URL.Query()
	page := pagination.FromContext(r.Context())

	query := searchquery.Parse(params.Get("q"))

	sortParam := params.Get("sort")
	sortField, sortDesc := parseSortParam(sortParam)

	var language string
	if lang := cmp.Or(query.Get("language"), query.Get("lang")); lang != nil {
		language = *lang
	}

	tf := searchquery.ExtractTextFilters(query)

	searchOpts := models.RepoSearchOptions{
		Keywords:        tf.Keywords,
		Phrases:         tf.Phrases,
		NegatedKeywords: tf.NegatedKeywords,
		NegatedPhrases:  tf.NegatedPhrases,
		Language:        language,
		SortField:       sortField,
		SortDesc:        sortDesc,
		Page:            page,
	}

	var repos []models.Repo
	var err error
	var resultCount int
	var searchDuration time.Duration
	var docCount int64
	method := "bleve"

	if searchOpts.HasSearchFilters() || sortParam != "" {
		res, err := s.indexer.Repos.Search(r.Context(), searchOpts)
		if err != nil {
			l.Error("failed to search repos", "err", err)
			s.pages.Error500(w)
			return
		}

		searchDuration = res.Duration

		if len(res.Hits) > 0 {
			repos, err = db.GetRepos(s.db, orm.FilterIn("id", res.Hits))
			if err != nil {
				l.Error("failed to get repos by IDs", "err", err)
				s.pages.Error500(w)
				return
			}

			hitIdx := make(map[int64]int, len(res.Hits))
			for i, id := range res.Hits {
				hitIdx[id] = i
			}
			slices.SortFunc(repos, func(a, b models.Repo) int {
				return cmp.Compare(hitIdx[a.Id], hitIdx[b.Id])
			})
		}
		resultCount = int(res.Total)

		dc, err := (s.indexer.Repos.TotalDocCount())
		if err != nil {
			l.Error("failed to get total doc count", "err", err)
		}
		docCount = int64(dc)

	} else {
		method = "db"
		repos, err = db.GetReposPaginated(
			s.db,
			page,
		)
		if err != nil {
			l.Error("failed to get repos", "err", err)
			s.pages.Error500(w)
			return
		}

		rc, err := db.CountRepos(
			s.db,
		)
		if err != nil {
			l.Error("failed to count repos", "err", err)
			s.pages.Error500(w)
			return
		}

		resultCount = int(rc)
		docCount = int64(rc)
	}

	l.Info(
		"RepoSearch",
		"method", method,
		"resultCount", resultCount,
		"docCount", docCount,
		"time", searchDuration,
		"filterQuery", query.String(),
		"sortParam", sortParam,
	)

	if !s.config.Core.Dev && query.String() != "" {
		distinctId := s.oauth.GetDid(r)
		if distinctId == "" {
			distinctId = "anonymous"
		}
		go func() {
			if err := s.posthog.Enqueue(posthog.Capture{
				DistinctId: distinctId,
				Event:      "search",
				Properties: posthog.Properties{
					"query":        query.String(),
					"result_count": resultCount,
					"method":       method,
				},
			}); err != nil {
				l.Error("failed to enqueue posthog event", "err", err)
			}
		}()
	}

	err = s.pages.SearchRepos(w, pages.SearchReposParams{
		LoggedInUser: s.oauth.GetMultiAccountUser(r),
		Repos:        repos,
		Page:         page,
		FilterQuery:  query.String(),
		SortParam:    sortParam,
		TimeTaken:    searchDuration,
		ResultCount:  resultCount,
		DocCount:     docCount,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
}

func (s *State) SearchQuick(w http.ResponseWriter, r *http.Request) {
	s.searchQuick(w, r, false)
}

func (s *State) SearchQuickMobile(w http.ResponseWriter, r *http.Request) {
	s.searchQuick(w, r, true)
}

func (s *State) searchQuick(w http.ResponseWriter, r *http.Request, mobile bool) {
	rawQuery := r.URL.Query().Get("q")
	if rawQuery == "" {
		w.WriteHeader(http.StatusOK)
		return
	}

	const pageSize = 5

	query := searchquery.Parse(rawQuery)
	tf := searchquery.ExtractTextFilters(query)

	searchOpts := models.RepoSearchOptions{
		Keywords:        tf.Keywords,
		Phrases:         tf.Phrases,
		NegatedKeywords: tf.NegatedKeywords,
		NegatedPhrases:  tf.NegatedPhrases,
		Page:            pagination.Page{Limit: pageSize},
	}

	var repos []models.Repo
	var total int

	if searchOpts.HasSearchFilters() {
		res, err := s.indexer.Repos.Search(r.Context(), searchOpts)
		if err != nil {
			s.logger.Error("failed quick search", "err", err)
			http.Error(w, "search failed", http.StatusInternalServerError)
			return
		}
		total = int(res.Total)
		if len(res.Hits) > 0 {
			repos, err = db.GetRepos(s.db, orm.FilterIn("id", res.Hits))
			if err != nil {
				s.logger.Error("failed to get repos for quick search", "err", err)
				http.Error(w, "search failed", http.StatusInternalServerError)
				return
			}
			hitIdx := make(map[int64]int, len(res.Hits))
			for i, id := range res.Hits {
				hitIdx[id] = i
			}
			slices.SortFunc(repos, func(a, b models.Repo) int {
				return cmp.Compare(hitIdx[a.Id], hitIdx[b.Id])
			})
		}
	}

	params := pages.SearchQuickParams{
		Repos: repos,
		Query: rawQuery,
		Total: total,
	}

	render := s.pages.SearchQuick
	if mobile {
		render = s.pages.SearchQuickMobile
	}
	if err := render(w, params); err != nil {
		s.logger.Error("failed to render quick search", "err", err)
	}
}

// parseSortParam parses sort parameter like "stars-desc" or "created-asc"
func parseSortParam(sortParam string) (string, bool) {
	defaultSort := func() (string, bool) { return "relevance", true }

	// no sort param supplied, just go default
	if sortParam == "" {
		return defaultSort()
	}

	parts := strings.Split(sortParam, "-")
	if len(parts) != 2 {
		return defaultSort()
	}

	field := parts[0]
	desc := parts[1] == "desc"

	// validate field
	validFields := map[string]bool{
		"relevance": true,
		"created":   true,
		"stars":     true,
		"issues":    true,
		"pulls":     true,
	}

	// invalid fields, just go default
	if !validFields[field] {
		return defaultSort()
	}

	return field, desc
}
