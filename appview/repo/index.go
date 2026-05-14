package repo

import (
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"

	"context"
	"encoding/json"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/commitverify"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/orm"
	"tangled.org/core/types"

	"github.com/go-chi/chi/v5"
	"github.com/go-enry/go-enry/v2"
)

func (rp *Repo) Index(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoIndex")

	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to fully resolve repo", "err", err)
		return
	}

	user := rp.oauth.GetMultiAccountUser(r)

	// Build index response from multiple XRPC calls
	result, err := rp.buildIndexResponse(r.Context(), f, ref)
	if err != nil {
		l.Error("failed to build index response", "err", err)
		rp.pages.RepoIndexPage(w, pages.RepoIndexParams{
			LoggedInUser:    user,
			KnotUnreachable: true,
			RepoInfo:        rp.repoResolver.GetRepoInfo(r, user),
		})
		return
	}

	tagMap := make(map[string][]string)
	for _, tag := range result.Tags {
		hash := tag.Hash
		if tag.Tag != nil {
			hash = tag.Tag.Target.String()
		}
		tagMap[hash] = append(tagMap[hash], tag.Name)
	}

	for _, branch := range result.Branches {
		hash := branch.Hash
		tagMap[hash] = append(tagMap[hash], branch.Name)
	}

	sortFiles(result.Files)

	slices.SortFunc(result.Branches, func(a, b types.Branch) int {
		if a.Name == result.Ref {
			return -1
		}
		if a.IsDefault {
			return -1
		}
		if b.IsDefault {
			return 1
		}
		if a.Commit != nil && b.Commit != nil {
			if a.Commit.Committer.When.Before(b.Commit.Committer.When) {
				return 1
			} else {
				return -1
			}
		}
		return strings.Compare(a.Name, b.Name) * -1
	})

	commitCount := len(result.Commits)
	branchCount := len(result.Branches)
	tagCount := len(result.Tags)
	fileCount := len(result.Files)

	commitCount, branchCount, tagCount = balanceIndexItems(commitCount, branchCount, tagCount, fileCount)
	commitsTrunc := result.Commits[:min(commitCount, len(result.Commits))]
	tagsTrunc := result.Tags[:min(tagCount, len(result.Tags))]
	branchesTrunc := result.Branches[:min(branchCount, len(result.Branches))]

	emails := uniqueEmails(commitsTrunc)
	emailToDidMap, err := db.GetEmailToDid(rp.db, emails, true)
	if err != nil {
		l.Error("failed to get email to did map", "err", err)
	}

	vc, err := commitverify.GetVerifiedCommits(rp.db, emailToDidMap, commitsTrunc)
	if err != nil {
		l.Error("failed to GetVerifiedObjectCommits", "err", err)
	}

	var languageInfo []types.RepoLanguageDetails
	if !result.IsEmpty {
		// TODO: a bit dirty
		langCtx, cancel := context.WithTimeout(r.Context(), 1*time.Second)
		defer cancel()
		languageInfo, err = rp.getLanguageInfo(langCtx, l, f, result.Ref, ref == "")
		if err != nil {
			l.Warn("failed to compute language percentages", "err", err)
			// non-fatal
		}
	}

	var shas []string
	for _, c := range commitsTrunc {
		shas = append(shas, c.Hash.String())
	}
	pipelines, err := getPipelineStatuses(rp.db, f, shas)
	if err != nil {
		l.Error("failed to fetch pipeline statuses", "err", err)
		// non-fatal
	}

	rp.pages.RepoIndexPage(w, pages.RepoIndexParams{
		LoggedInUser:      user,
		RepoInfo:          rp.repoResolver.GetRepoInfo(r, user),
		TagMap:            tagMap,
		RepoIndexResponse: *result,
		CommitsTrunc:      commitsTrunc,
		TagsTrunc:         tagsTrunc,
		// ForkInfo:           forkInfo, // TODO: reinstate this after xrpc properly lands
		BranchesTrunc:   branchesTrunc,
		EmailToDid:      emailToDidMap,
		VerifiedCommits: vc,
		Languages:       languageInfo,
		Pipelines:       pipelines,
	})
}

func (rp *Repo) getLanguageInfo(
	ctx context.Context,
	l *slog.Logger,
	repo *models.Repo,
	currentRef string,
	isDefaultRef bool,
) ([]types.RepoLanguageDetails, error) {
	// first attempt to fetch from db
	langs, err := db.GetRepoLanguages(
		rp.db,
		orm.FilterEq("repo_did", repo.RepoDid),
		orm.FilterEq("ref", currentRef),
	)

	if err != nil || langs == nil || containsOtherLanguage(langs) {
		langs = nil

		// non-fatal, fetch langs from ks via XRPC
		xrpcc := &indigoxrpc.Client{
			Host:   rp.config.KnotMirror.Url,
			Client: http.DefaultClient,
		}
		ls, err := tangled.GitTempListLanguages(ctx, xrpcc, currentRef, repo.RepoDid)
		if err != nil {
			return nil, fmt.Errorf("calling knotmirror git.listLanguages: %w", err)
		}

		if ls == nil || ls.Languages == nil {
			return nil, nil
		}

		for _, lang := range ls.Languages {
			langs = append(langs, models.RepoLanguage{
				RepoDid:      syntax.DID(repo.RepoDid),
				Ref:          currentRef,
				IsDefaultRef: isDefaultRef,
				Language:     lang.Name,
				Bytes:        lang.Size,
			})
		}

		tx, err := rp.db.Begin()
		if err != nil {
			return nil, err
		}
		defer tx.Rollback()

		// update appview's cache
		err = db.UpdateRepoLanguages(tx, syntax.DID(repo.RepoDid), currentRef, langs)
		if err != nil {
			// non-fatal
			l.Error("failed to cache lang results", "err", err)
		}

		err = tx.Commit()
		if err != nil {
			return nil, err
		}
	}

	var total int64
	for _, l := range langs {
		total += l.Bytes
	}

	var languageStats []types.RepoLanguageDetails
	for _, l := range langs {
		percentage := float32(l.Bytes) / float32(total) * 100
		color := enry.GetColor(l.Language)
		languageStats = append(languageStats, types.RepoLanguageDetails{
			Name:       l.Language,
			Percentage: percentage,
			Color:      color,
		})
	}

	sort.Slice(languageStats, func(i, j int) bool {
		if languageStats[i].Name == enry.OtherLanguage {
			return false
		}
		if languageStats[j].Name == enry.OtherLanguage {
			return true
		}
		if languageStats[i].Percentage != languageStats[j].Percentage {
			return languageStats[i].Percentage > languageStats[j].Percentage
		}
		return languageStats[i].Name < languageStats[j].Name
	})

	return languageStats, nil
}

func containsOtherLanguage(langs []models.RepoLanguage) bool {
	for _, l := range langs {
		if l.Language == enry.OtherLanguage {
			return true
		}
	}
	return false
}

// buildIndexResponse creates a RepoIndexResponse by combining multiple xrpc calls in parallel
func (rp *Repo) buildIndexResponse(ctx context.Context, repo *models.Repo, ref string) (*types.RepoIndexResponse, error) {
	xrpcc := &indigoxrpc.Client{Host: rp.config.KnotMirror.Url}

	branchesBytes, err := tangled.GitTempListBranches(ctx, xrpcc, "", 0, repo.RepoDid)
	if err != nil {
		return nil, fmt.Errorf("calling knotmirror git.listBranches: %w", err)
	}

	var branchesResp types.RepoBranchesResponse
	if err := json.Unmarshal(branchesBytes, &branchesResp); err != nil {
		return nil, fmt.Errorf("failed to unmarshal branches response: %w", err)
	}

	// if no ref specified, use default branch or first available
	if ref == "" {
		for _, branch := range branchesResp.Branches {
			if branch.IsDefault {
				ref = branch.Name
				break
			}
		}
	}

	// if ref is still empty, this means the default branch is not set
	if ref == "" {
		return &types.RepoIndexResponse{
			IsEmpty:  true,
			Branches: branchesResp.Branches,
		}, nil
	}

	// now run the remaining queries in parallel
	var wg sync.WaitGroup
	var errs error

	var (
		tagsResp       types.RepoTagsResponse
		treeResp       *tangled.GitTempGetTree_Output
		logResp        types.RepoLogResponse
		readmeContent  string
		readmeFileName string
	)

	// tags
	wg.Go(func() {
		tagsBytes, err := tangled.GitTempListTags(ctx, xrpcc, "", 0, repo.RepoDid)
		if err != nil {
			errs = errors.Join(errs, fmt.Errorf("failed to call git.ListTags: %w", err))
			return
		}

		if err := json.Unmarshal(tagsBytes, &tagsResp); err != nil {
			errs = errors.Join(errs, fmt.Errorf("failed to unmarshal git.ListTags: %w", err))
		}
	})

	// tree/files
	wg.Go(func() {
		resp, err := tangled.GitTempGetTree(ctx, xrpcc, "", ref, repo.RepoDid)
		if err != nil {
			errs = errors.Join(errs, fmt.Errorf("failed to call git.GetTree: %w", err))
			return
		}
		treeResp = resp
	})

	// commits
	wg.Go(func() {
		logBytes, err := tangled.GitTempListCommits(ctx, xrpcc, "", 50, ref, repo.RepoDid)
		if err != nil {
			errs = errors.Join(errs, fmt.Errorf("failed to call git.ListCommits: %w", err))
			return
		}

		if err := json.Unmarshal(logBytes, &logResp); err != nil {
			errs = errors.Join(errs, fmt.Errorf("failed to unmarshal git.ListCommits: %w", err))
		}
	})

	wg.Wait()

	if errs != nil {
		return nil, errs
	}

	var files []types.NiceTree
	if treeResp != nil && treeResp.Files != nil {
		for _, file := range treeResp.Files {
			niceFile := types.NiceTree{
				Name: file.Name,
				Mode: file.Mode,
				Size: file.Size,
			}

			if file.Last_commit != nil {
				when, _ := time.Parse(time.RFC3339, file.Last_commit.When)
				niceFile.LastCommit = &types.LastCommitInfo{
					Hash:    plumbing.NewHash(file.Last_commit.Hash),
					Message: file.Last_commit.Message,
					When:    when,
				}
			}
			files = append(files, niceFile)
		}
	}

	if treeResp != nil && treeResp.Readme != nil {
		readmeFileName = treeResp.Readme.Filename
		readmeContent = treeResp.Readme.Contents
	}

	result := &types.RepoIndexResponse{
		IsEmpty:        false,
		Ref:            ref,
		Readme:         readmeContent,
		ReadmeFileName: readmeFileName,
		Commits:        logResp.Commits,
		Description:    "",
		Files:          files,
		Branches:       branchesResp.Branches,
		Tags:           tagsResp.Tags,
		TotalCommits:   logResp.Total,
	}

	return result, nil
}
