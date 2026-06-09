package repo

import (
	"errors"
	"fmt"
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
	"tangled.org/core/appview/pages/markup"
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

	if user != nil {
		userDid := user.Did
		repoDid := f.RepoDid
		go func() {
			if err := db.UpsertRecentLink(rp.db, userDid, models.RecentLinkTypeRepo, repoDid); err != nil {
				l.Error("failed to upsert recent link", "err", err)
			}
		}()
	}

	// Build index response from multiple XRPC calls
	result, err := rp.buildIndexResponse(r.Context(), f, ref)
	if err != nil {
		l.Error("failed to build index response", "err", err)
		rp.pages.RepoIndexPage(w, pages.RepoIndexParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
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
		langs, err := rp.getLanguageInfo(r.Context(), syntax.DID(f.RepoDid), result.Ref)
		if err != nil {
			l.Warn("failed to compute language percentages", "err", err)
			// non-fatal
		} else if ref == "" { // when request didn't specified ref, we are fetching default branch.
			if err := func(repo syntax.DID, ref string, langs []*tangled.GitTempListLanguages_Language) error {
				tx, err := rp.db.Begin()
				if err != nil {
					return err
				}
				defer tx.Rollback()

				var mlangs []models.RepoLanguage
				for _, lang := range langs {
					mlangs = append(mlangs, models.RepoLanguage{
						RepoDid:      repo,
						Ref:          ref,
						IsDefaultRef: true,
						Language:     lang.Name,
						Bytes:        lang.Size,
					})
				}

				if err := db.UpdateRepoLanguages(tx, syntax.DID(f.RepoDid), ref, mlangs); err != nil {
					return err
				}

				return tx.Commit()
			}(syntax.DID(f.RepoDid), result.Ref, langs); err != nil {
				l.Error("failed to populate appview repo languages index", "err", err)
				// non-fatal
			}
			languageInfo = makeLanguageStats(langs)
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
		BaseParams: pages.BaseParamsFromContext(r.Context()),
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
	repoId syntax.DID,
	ref string,
) ([]*tangled.GitTempListLanguages_Language, error) {
	// non-fatal, fetch langs from knotmirror via XRPC
	xrpcc := &indigoxrpc.Client{
		Host:   rp.config.KnotMirror.Url,
		Client: http.DefaultClient,
	}
	out, err := tangled.GitTempListLanguages(ctx, xrpcc, ref, repoId.String())
	if err != nil {
		return nil, fmt.Errorf("calling knotmirror git.listLanguages: %w", err)
	}

	if out == nil || out.Languages == nil {
		return nil, nil
	}

	return out.Languages, nil
}

func makeLanguageStats(langs []*tangled.GitTempListLanguages_Language) []types.RepoLanguageDetails {
	if len(langs) == 0 {
		return nil
	}
	var total int64
	for _, lang := range langs {
		total += lang.Size
	}

	var languageStats []types.RepoLanguageDetails
	for _, l := range langs {
		languageStats = append(languageStats, types.RepoLanguageDetails{
			Name:       l.Name,
			Color:      enry.GetColor(l.Name),
			Percentage: float32(l.Size) / float32(total) * 100,
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
	return languageStats
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

		for _, file := range resp.Files {
			if markup.IsReadmeFile(file.Name, file.Mode) {
				readmeFileName = file.Name
				break
			}
		}

		if readmeFileName != "" {
			bytes, err := tangled.GitTempGetBlob(ctx, xrpcc, readmeFileName, ref, repo.RepoDid)
			if err != nil {
				errs = errors.Join(errs, fmt.Errorf("failed to call git.getBlob: %w", err))
				return
			}
			readmeContent = string(bytes)
		}
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

	result := &types.RepoIndexResponse{
		IsEmpty:        false,
		Ref:            ref,
		Readme:         readmeContent,
		ReadmeFileName: readmeFileName,
		Commits:        logResp.Commits,
		Files:          files,
		Branches:       branchesResp.Branches,
		Tags:           tagsResp.Tags,
		TotalCommits:   logResp.Total,
	}

	return result, nil
}
