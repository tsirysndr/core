package repo

import (
	"context"
	"maps"
	"slices"
	"sort"
	"strings"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/hostutil"
	"tangled.org/core/types"
)

func sortFiles(files []types.NiceTree) {
	sort.Slice(files, func(i, j int) bool {
		iIsFile := files[i].IsFile()
		jIsFile := files[j].IsFile()
		if iIsFile != jIsFile {
			return !iIsFile
		}
		return files[i].Name < files[j].Name
	})
}

func sortBranches(branches []types.Branch) {
	slices.SortFunc(branches, func(a, b types.Branch) int {
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
		return strings.Compare(a.Name, b.Name)
	})
}

func uniqueEmails(commits []types.Commit) []string {
	emails := make(map[string]struct{})
	for _, commit := range commits {
		emails[commit.Author.Email] = struct{}{}
		emails[commit.Committer.Email] = struct{}{}
		for _, c := range commit.CoAuthors() {
			emails[c.Email] = struct{}{}
		}
	}

	// delete empty emails if any, from the set
	delete(emails, "")

	return slices.Collect(maps.Keys(emails))
}

func balanceIndexItems(commitCount, branchCount, tagCount, fileCount int) (commitsTrunc int, branchesTrunc int, tagsTrunc int) {
	if commitCount == 0 && tagCount == 0 && branchCount == 0 {
		return
	}

	// typically 1 item on right side = 2 files in height
	availableSpace := fileCount / 2

	// clamp tagcount
	if tagCount > 0 {
		tagsTrunc = 1
		availableSpace -= 1 // an extra subtracted for headers etc.
	}

	// clamp branchcount
	if branchCount > 0 {
		branchesTrunc = min(max(branchCount, 1), 3)
		availableSpace -= branchesTrunc // an extra subtracted for headers etc.
	}

	// show
	if commitCount > 0 {
		commitsTrunc = max(availableSpace, 3)
	}

	return
}

// fetch pipelines from spindle and map by commit sha
func getPipelineStatuses(
	ctx context.Context,
	repo *models.Repo,
	shas []string,
) (map[string]types.Pipeline, error) {
	m := make(map[string]types.Pipeline)

	if len(shas) == 0 {
		return m, nil
	}

	if repo.Spindle == "" {
		return m, nil
	}

	spindleUrl, err := hostutil.EnsureHttpScheme(repo.Spindle)
	if err != nil {
		return m, nil
	}

	xrpcc := &indigoxrpc.Client{Host: spindleUrl}
	out, err := tangled.CiQueryPipelines(ctx, xrpcc, shas, "", nil, 0, repo.RepoDid)
	if err != nil {
		return nil, err
	}

	for _, p := range out.Pipelines {
		m[p.Commit] = types.Pipeline{CiPipeline: p}
	}

	return m, nil
}
