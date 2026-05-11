package xrpc

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
)

const (
	RepoLanguagesByDid = "git_list_languages:repo:%s:%s"
	RepoLanguagesTTL   = 24 * time.Hour
)

func (x *Xrpc) ListLanguages(w http.ResponseWriter, r *http.Request) {
	var (
		repoQuery = r.URL.Query().Get("repo")
		ref       = r.URL.Query().Get("ref")
	)
	l := x.logger.With("method", "git.listLanguages", "repo", repoQuery, "ref", ref)

	repo, err := syntax.ParseATURI(repoQuery)
	if err != nil || repo.RecordKey() == "" {
		l.Error("invalid repo at-uri", "err", err)
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", repoQuery)})
		return
	}

	if val, err := x.rdb.Get(r.Context(), fmt.Sprintf(RepoLanguagesByDid, repo, ref)).Result(); err == nil {
		l.Debug("served from cache")
		var langs []*tangled.GitTempListLanguages_Language
		err = json.Unmarshal([]byte(val), &langs)
		if err == nil {
			writeJson(w, http.StatusOK, &tangled.GitTempListLanguages_Output{
				Ref:       ref,
				Languages: langs,
			})
			return
		}
	}

	out, err := x.listLanguages(r.Context(), repo, ref)
	if err != nil {
		l.Warn("local mirror failed, trying proxy", "err", err)
		if x.proxyToKnot(w, r, repo) {
			return
		}
		writeErr(w, err)
		return
	}

	writeJson(w, http.StatusOK, out)
}

func (x *Xrpc) listLanguages(ctx context.Context, repo syntax.ATURI, ref string) (*tangled.GitTempListLanguages_Output, error) {
	repoPath, err := x.makeRepoPath(ctx, repo)
	if err != nil {
		return nil, fmt.Errorf("resolving repo at-uri: %w", err)
	}

	gr, err := git.Open(repoPath, ref)
	if err != nil {
		return nil, &atclient.APIError{StatusCode: http.StatusNotFound, Name: "RepoNotFound", Message: "failed to find git repo"}
	}

	ctx, cancel := context.WithTimeout(ctx, 1*time.Second)
	defer cancel()

	sizes, err := gr.AnalyzeLanguages(ctx)
	if err != nil {
		return nil, fmt.Errorf("analyzing languages: %w", err)
	}

	langs := sizesToLanguages(sizes)

	go func() {
		ctx := context.Background()
		encoded, err := json.Marshal(langs)
		if err != nil {
			return
		}
		x.rdb.Set(ctx, fmt.Sprintf(RepoLanguagesByDid, repo, ref), encoded, RepoLanguagesTTL)
	}()

	return &tangled.GitTempListLanguages_Output{
		Ref:       ref,
		Languages: sizesToLanguages(sizes),
	}, nil
}

func sizesToLanguages(sizes git.LangBreakdown) []*tangled.GitTempListLanguages_Language {
	var apiLanguages []*tangled.GitTempListLanguages_Language
	var totalSize int64
	for _, size := range sizes {
		totalSize += size
	}

	for name, size := range sizes {
		percentagef64 := float64(size) / float64(totalSize) * 100
		percentage := math.Round(percentagef64)

		lang := &tangled.GitTempListLanguages_Language{
			Name:       name,
			Size:       size,
			Percentage: int64(percentage),
		}

		apiLanguages = append(apiLanguages, lang)
	}

	return apiLanguages
}
