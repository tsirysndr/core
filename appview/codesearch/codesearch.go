package codesearch

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sourcegraph/zoekt"
	"github.com/sourcegraph/zoekt/query"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
)

type CodeSearch struct {
	Host   string // zoekt-webserver host. example: https://zoekt.example.com
	Client *http.Client
}

func (s *CodeSearch) GetClient() *http.Client {
	if s.Client != nil {
		return s.Client
	}
	return http.DefaultClient
}

type RepoOnlyError struct{ Query string }

func (e *RepoOnlyError) Error() string {
	return "query only filters by repo name; use repo search instead"
}

// jsonSearchArgs mirrors zoekt's /api/search request body.
type jsonSearchArgs struct {
	Q    string
	Opts *zoekt.SearchOptions
}

// jsonSearchReply mirrors zoekt's /api/search response body.
type jsonSearchReply struct {
	Result *zoekt.SearchResult
}

// jsonListArgs mirrors zoekt's /api/list request body.
type jsonListArgs struct {
	Q    string
	Opts *zoekt.ListOptions
}

// jsonListReply mirrors zoekt's /api/list response body.
type jsonListReply struct {
	List *zoekt.RepoList
}

// SearchResults is a single page of content-search results plus whether more
// pages follow.
type SearchResults struct {
	Results []models.Result
	HasMore bool
	Stats   zoekt.Stats // zoekt search stats (MatchCount, FileCount, Duration, …)
}

// Search queries zoekt server for FileNameMatch or ChunkMatch.
// It returns *RepoOnlyError when the query only filters by repo name
// (optionally with `lang:` filter.)
func (s *CodeSearch) Search(ctx context.Context, queryStr string, page pagination.Page) (*SearchResults, error) {
	q, err := query.Parse(queryStr)
	if err != nil {
		return nil, fmt.Errorf("parse query: %w", err)
	}
	if rs, ok := asRepoSearch(q); ok {
		return nil, &RepoOnlyError{Query: rs.Query()}
	}

	opts := &zoekt.SearchOptions{
		ChunkMatches:    true,
		MaxWallTime:     10 * time.Second,
		NumContextLines: 2,
	}
	if page.Limit > 0 {
		// +1 so we can detect a following page.
		opts.MaxDocDisplayCount = page.Offset + page.Limit + 1
	}

	body, err := json.Marshal(jsonSearchArgs{
		Q:    queryStr,
		Opts: opts,
	})
	if err != nil {
		return nil, fmt.Errorf("marshal request: %w", err)
	}

	url := strings.TrimRight(s.Host, "/") + "/api/search"
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("build request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := s.GetClient().Do(req)
	if err != nil {
		return nil, fmt.Errorf("do request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return nil, fmt.Errorf("zoekt search: status %d: %s", resp.StatusCode, strings.TrimSpace(string(b)))
	}

	var reply jsonSearchReply
	if err := json.NewDecoder(resp.Body).Decode(&reply); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	if reply.Result == nil {
		return &SearchResults{}, nil
	}

	stats := reply.Result.Stats
	all := toResults(reply.Result)
	end := page.Offset + page.Limit
	if page.Limit <= 0 {
		// No window requested: return everything.
		return &SearchResults{Results: all, Stats: stats}, nil
	}

	hasMore := len(all) > end // extra card present ⇒ more pages
	if page.Offset >= len(all) {
		return &SearchResults{HasMore: false, Stats: stats}, nil
	}
	if end > len(all) {
		end = len(all)
	}
	return &SearchResults{Results: all[page.Offset:end], HasMore: hasMore, Stats: stats}, nil
}

// RepoCount returns the total number of repositories in the zoekt index.
func (s *CodeSearch) RepoCount(ctx context.Context) (int, error) {
	body, err := json.Marshal(jsonListArgs{
		Q:    "", // empty query ⇒ match all repos
		Opts: &zoekt.ListOptions{Field: zoekt.RepoListFieldRepos},
	})
	if err != nil {
		return 0, fmt.Errorf("marshal request: %w", err)
	}

	url := strings.TrimRight(s.Host, "/") + "/api/list"
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return 0, fmt.Errorf("build request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := s.GetClient().Do(req)
	if err != nil {
		return 0, fmt.Errorf("do request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return 0, fmt.Errorf("zoekt list: status %d: %s", resp.StatusCode, strings.TrimSpace(string(b)))
	}

	var reply jsonListReply
	if err := json.NewDecoder(resp.Body).Decode(&reply); err != nil {
		return 0, fmt.Errorf("decode response: %w", err)
	}
	if reply.List == nil {
		return 0, nil
	}
	return reply.List.Stats.Repos, nil
}

// toResults maps zoekt FileMatches into local Results
func toResults(sr *zoekt.SearchResult) []models.Result {
	var out []models.Result
	for _, fm := range sr.Files {
		// HACK: zoekt use int64 repo.ID as identifier, but we expect DID (string) as an repo identifier.
		// as a quick hack without patching zoekt, we extract the DID from RepoURLs
		repoDID := extractDID(sr.RepoURLs[fm.Repository])
		res := models.Result{
			RepoDID:  repoDID,
			FilePath: fm.FileName,
			Branches: fm.Branches,
			Commit:   fm.Version,
			Language: fm.Language,
		}
		for _, cm := range fm.ChunkMatches {
			if cm.FileName {
				res.File = &models.Result_FileMatch{Ranges: cm.Ranges}
				break
			} else {
				res.Chunks = append(res.Chunks, models.Result_ChunkMatch{
					Content:          string(cm.Content),
					ContentStartLine: int(cm.ContentStart.LineNumber),
					Ranges:           cm.Ranges,
				})
			}
		}
		out = append(out, res)
	}
	return out
}

// extractDID pulls the repo DID out of a zoekt FileURLTemplate of the form
// "{appviewURL}/{repoDID}/blob/{commit}/{path}".
func extractDID(urlTemplate string) syntax.DID {
	if urlTemplate == "" {
		return ""
	}
	u, err := url.Parse(urlTemplate)
	if err != nil {
		return ""
	}
	seg := strings.SplitN(strings.TrimPrefix(u.Path, "/"), "/", 2)[0]
	return syntax.DID(seg)
}

type repoSearchQuery struct {
	RepoNames []string
	Language  string
}

func (r repoSearchQuery) Query() string {
	parts := append([]string{}, r.RepoNames...)
	if r.Language != "" {
		parts = append(parts, "lang:"+r.Language)
	}
	return strings.Join(parts, " ")
}

func asRepoSearch(q query.Q) (repoSearchQuery, bool) {
	var rs repoSearchQuery
	if t, ok := q.(*query.Type); ok && t.Type == query.TypeRepo {
		query.VisitAtoms(t.Child, func(a query.Q) {
			switch v := a.(type) {
			case *query.Repo:
				rs.RepoNames = append(rs.RepoNames, v.Regexp.String())
			case *query.Substring:
				rs.RepoNames = append(rs.RepoNames, v.Pattern)
			case *query.Language:
				rs.Language = v.Language
			}
		})
		return rs, true
	}
	hasRepo, only := false, true
	query.VisitAtoms(q, func(a query.Q) {
		switch v := a.(type) {
		case *query.Repo:
			hasRepo = true
			rs.RepoNames = append(rs.RepoNames, v.Regexp.String())
		case *query.Language:
			rs.Language = v.Language
		default:
			only = false
		}
	})
	return rs, hasRepo && only
}
