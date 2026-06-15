package state

import (
	"errors"
	"net/http"
	"net/url"
	"slices"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/codesearch"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func (s *State) handleCodeSearch(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "CodeSearch")
	ctx := r.Context()
	page := pagination.FromContext(ctx)
	q := r.URL.Query().Get("q")

	var redirected bool
	var params pages.CodeSearchParams
	params.BaseParams = pages.BaseParamsFromContext(ctx)
	params.FilterQuery = q
	params.Page = page
	defer func() {
		if redirected {
			return
		}
		if err := s.pages.CodeSearch(w, params); err != nil {
			l.Error("failed to render code search", "err", err)
		}
	}()

	if q == "" {
		return
	}

	res, err := s.codesearch.Search(ctx, q, page)
	if err != nil {
		// repo-name-only queries belong to the repo search page; redirect with
		// the rewritten query (repo: prefix dropped, lang: kept).
		var repoErr *codesearch.RepoOnlyError
		if errors.As(err, &repoErr) {
			redirected = true
			http.Redirect(w, r, "/search?q="+url.QueryEscape(repoErr.Query), http.StatusFound)
			return
		}
		l.Error("code search failed", "err", err, "query", q)
		params.ErrorMsg = "Failed to perform search. Please try again later."
		return
	}
	results := res.Results

	repoMap := map[syntax.DID]*models.Repo{}
	var repoDids []string
	for _, res := range results {
		if res.RepoDID == "" {
			continue
		}
		if _, ok := repoMap[res.RepoDID]; !ok {
			repoMap[res.RepoDID] = nil
			repoDids = append(repoDids, res.RepoDID.String())
		}
	}
	if len(repoDids) > 0 {
		repos, err := db.GetRepos(s.db, orm.FilterIn("repo_did", repoDids))
		if err != nil {
			l.Error("failed to load repos for code search", "err", err)
			params.ErrorMsg = "Failed to load repos for code search. Please try again later."
			return
		}
		for i := range repos {
			repoMap[syntax.DID(repos[i].RepoDid)] = &repos[i]
		}
	}

	out := make([]pages.SearchResult, 0, len(results))
	for _, res := range results {
		csr := pages.SearchResult{
			RepoDID:  res.RepoDID,
			Repo:     repoMap[res.RepoDID],
			FilePath: res.FilePath,
			Branches: res.Branches,
			Commit:   res.Commit,
			Language: res.Language,
		}
		if f := res.File; f != nil {
			csr.File = &pages.CodeSearchResult_File{
				NameSpans: pages.FileNameSpans(res.FilePath, f.Ranges),
			}
		}
		slices.SortStableFunc(res.Chunks, func(a, b models.Result_ChunkMatch) int {
			return a.ContentStartLine - b.ContentStartLine
		})
		for _, c := range res.Chunks {
			csr.Chunks = append(csr.Chunks, pages.CodeSearchResult_Chunk{
				Lines:      pages.ChunkLines(c.Content, c.ContentStartLine, c.Ranges),
				MatchCount: len(c.Ranges),
			})
		}
		out = append(out, csr)
	}

	params.Results = out
	params.HasMore = res.HasMore
	params.MatchCount = res.Stats.MatchCount
	params.FileCount = res.Stats.FileCount
	params.TimeTaken = res.Stats.Duration
}
