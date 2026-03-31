package state

import (
	"net/http"
	"strings"
	"time"

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
	if lang := query.Get("language"); lang != nil {
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

	if searchOpts.HasSearchFilters() || sortField != "" {
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

			// sort repos to match search result order (by relevance)
			repoMap := make(map[int64]models.Repo, len(repos))
			for _, repo := range repos {
				repoMap[repo.Id] = repo
			}
			repos = make([]models.Repo, 0, len(res.Hits))
			for _, id := range res.Hits {
				if repo, ok := repoMap[id]; ok {
					repos = append(repos, repo)
				}
			}
		}
		resultCount = int(res.Total)

		dc, err := (s.indexer.Repos.TotalDocCount())
		if err != nil {
			l.Error("failed to get total doc count", "err", err)
		}
		docCount = int64(dc)

	} else {
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
