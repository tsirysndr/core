package repo

import (
	"fmt"
	"net/http"
	"slices"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
)

func (rp *Repo) Search(w http.ResponseWriter, r *http.Request) {
	if r.Header.Get("Hx-Request") == "true" {
		rp.searchResultsFragment(w, r)
		return
	}
	if err := rp.pages.RepoSearchPage(w, pages.RepoSearchParams{
		BaseParams:  pages.BaseParamsFromContext(r.Context()),
		RepoInfo:    rp.repoResolver.GetRepoInfo(r, rp.oauth.GetMultiAccountUser(r)),
		FilterQuery: "",
	}); err != nil {
		rp.logger.Error("failed to render", "err", err)
	}
}

func (rp *Repo) searchResultsFragment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "Search")
	repo, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo", "err", err)
		return
	}
	q := r.URL.Query().Get("q")

	var params pages.RepoSearchResultsFragmentParams
	defer func() {
		if err := rp.pages.RepoSearchResultsFragment(w, params); err != nil {
			l.Error("failed to render", "err", err)
		}
	}()

	if q == "" {
		return
	}

	q = fmt.Sprintf(`meta.did:%s %q`, repo.RepoDid, q)

	ctx := r.Context()

	res, err := rp.codesearch.Search(ctx, q, pagination.Page{Limit: 50})
	if err != nil {
		l.Error("failed to search files", "err", err)
		params.ErrorMsg = "Failed to perform search. Please try again later."
		return
	}

	if len(res.Results) == 0 {
		return
	}

	out := make([]pages.SearchResult, 0, len(res.Results))
	for _, res := range res.Results {
		if res.RepoDID != syntax.DID(repo.RepoDid) {
			continue
		}
		csr := pages.SearchResult{
			RepoDID:  res.RepoDID,
			Repo:     repo,
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

	l.Debug("repo file search result", "len", len(out), "duration", res.Stats.Duration)
	params.Results = out
}
