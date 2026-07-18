package repo

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"slices"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/gorilla/feeds"
)

// which types of items to include in the feed.
type FeedOpts struct {
	IncludeIssues  bool
	IncludePulls   bool
	IncludeCommits bool
	IncludeTags    bool
}

func parseFeedOpts(r *http.Request) FeedOpts {
	includeParam := r.URL.Query().Get("include")

	// default: include everything
	if includeParam == "" {
		return FeedOpts{
			IncludeIssues:  true,
			IncludePulls:   true,
			IncludeCommits: true,
			IncludeTags:    true,
		}
	}

	// parse comma-separated list
	opts := FeedOpts{}
	types := strings.SplitSeq(includeParam, ",")
	for t := range types {
		switch strings.TrimSpace(strings.ToLower(t)) {
		case "issues":
			opts.IncludeIssues = true
		case "pulls", "prs":
			opts.IncludePulls = true
		case "commits":
			opts.IncludeCommits = true
		case "tags":
			opts.IncludeTags = true
		}
	}

	return opts
}

func (rp *Repo) getRepoFeed(ctx context.Context, repo *models.Repo, ownerSlashRepo string, opts FeedOpts) (*feeds.Feed, error) {
	feedPagePerType := pagination.Page{Limit: 100}

	feed := &feeds.Feed{
		Title:   fmt.Sprintf("activity feed for @%s", ownerSlashRepo),
		Link:    &feeds.Link{Href: fmt.Sprintf("%s/%s", rp.config.Core.BaseUrl(), ownerSlashRepo), Type: "text/html", Rel: "alternate"},
		Items:   make([]*feeds.Item, 0),
		Updated: time.UnixMilli(0),
	}

	// fetch and add pull requests if requested
	if opts.IncludePulls {
		pulls, err := db.GetPullsPaginated(rp.db, feedPagePerType, orm.FilterEq("repo_did", repo.RepoDid))
		if err != nil {
			return nil, err
		}

		for _, pull := range pulls {
			items, err := rp.createPullItems(ctx, pull, ownerSlashRepo)
			if err != nil {
				return nil, err
			}
			feed.Items = append(feed.Items, items...)
		}
	}

	// fetch and add issues if requested
	if opts.IncludeIssues {
		issues, err := db.GetIssuesPaginated(
			rp.db,
			feedPagePerType,
			orm.FilterEq("repo_did", repo.RepoDid),
		)
		if err != nil {
			return nil, err
		}

		for _, issue := range issues {
			item, err := rp.createIssueItem(ctx, issue, ownerSlashRepo)
			if err != nil {
				return nil, err
			}
			feed.Items = append(feed.Items, item)
		}
	}

	// fetch and add commits if requested
	if opts.IncludeCommits {
		commitItems, err := rp.createCommitItems(ctx, repo, ownerSlashRepo)
		if err != nil {
			// Soft failure: log error and continue with partial feed
			rp.logger.Error("failed to fetch commits for feed", "err", err)
		} else {
			feed.Items = append(feed.Items, commitItems...)
		}
	}

	// fetch and add tags if requested
	if opts.IncludeTags {
		tagItems, err := rp.createTagItems(ctx, repo, ownerSlashRepo)
		if err != nil {
			// Soft failure: log error and continue with partial feed
			rp.logger.Error("failed to fetch tags for feed", "err", err)
		} else {
			feed.Items = append(feed.Items, tagItems...)
		}
	}

	slices.SortFunc(feed.Items, func(a, b *feeds.Item) int {
		if a.Created.After(b.Created) {
			return -1
		}
		return 1
	})

	if len(feed.Items) > 100 {
		feed.Items = feed.Items[:100]
	}

	if len(feed.Items) > 0 {
		feed.Updated = feed.Items[0].Created
	}

	return feed, nil
}

func (rp *Repo) createPullItems(ctx context.Context, pull *models.Pull, ownerSlashRepo string) ([]*feeds.Item, error) {
	owner, err := rp.idResolver.ResolveIdent(ctx, pull.OwnerDid)
	if err != nil {
		return nil, err
	}

	var items []*feeds.Item

	state := rp.getPullState(pull)
	description := rp.buildPullDescription(owner.Handle, state, pull, ownerSlashRepo)

	mainItem := &feeds.Item{
		Title:       fmt.Sprintf("[PR #%d] %s", pull.PullId, pull.Title),
		Description: description,
		Link:        &feeds.Link{Href: fmt.Sprintf("%s/%s/pulls/%d", rp.config.Core.BaseUrl(), ownerSlashRepo, pull.PullId)},
		Created:     pull.Created,
		Author:      &feeds.Author{Name: fmt.Sprintf("%s", owner.Handle)},
	}
	items = append(items, mainItem)

	for _, round := range pull.Submissions {
		if round == nil || round.RoundNumber == 0 {
			continue
		}

		roundItem := &feeds.Item{
			Title:       fmt.Sprintf("[PR #%d] %s (round #%d)", pull.PullId, pull.Title, round.RoundNumber),
			Description: fmt.Sprintf("%s submitted changes (at round #%d) on PR #%d in %s", owner.Handle, round.RoundNumber, pull.PullId, ownerSlashRepo),
			Link:        &feeds.Link{Href: fmt.Sprintf("%s/%s/pulls/%d/round/%d/", rp.config.Core.BaseUrl(), ownerSlashRepo, pull.PullId, round.RoundNumber)},
			Created:     round.Created,
			Author:      &feeds.Author{Name: fmt.Sprintf("@%s", owner.Handle)},
		}
		items = append(items, roundItem)
	}

	return items, nil
}

func (rp *Repo) createIssueItem(ctx context.Context, issue models.Issue, ownerSlashRepo string) (*feeds.Item, error) {
	owner, err := rp.idResolver.ResolveIdent(ctx, issue.Did)
	if err != nil {
		return nil, err
	}

	state := "closed"
	if issue.Open {
		state = "opened"
	}

	return &feeds.Item{
		Title:       fmt.Sprintf("[Issue #%d] %s", issue.IssueId, issue.Title),
		Description: fmt.Sprintf("%s %s issue #%d in %s", owner.Handle, state, issue.IssueId, ownerSlashRepo),
		Link:        &feeds.Link{Href: fmt.Sprintf("%s/%s/issues/%d", rp.config.Core.BaseUrl(), ownerSlashRepo, issue.IssueId)},
		Created:     issue.Created,
		Author:      &feeds.Author{Name: owner.Handle.String()},
	}, nil
}

func (rp *Repo) createCommitItems(ctx context.Context, repo *models.Repo, ownerSlashRepo string) ([]*feeds.Item, error) {
	xrpcc := rp.knotMirrorXRPCClient()

	xrpcBytes, err := tangled.GitTempListCommits(ctx, xrpcc, "", 100, "", repo.RepoDid)
	if err != nil {
		return nil, fmt.Errorf("failed to call XRPC repo.log: %w", err)
	}

	var xrpcResp types.RepoLogResponse
	if err := json.Unmarshal(xrpcBytes, &xrpcResp); err != nil {
		return nil, fmt.Errorf("failed to decode XRPC response: %w", err)
	}

	var items []*feeds.Item
	for _, commit := range xrpcResp.Commits {
		messageLines := strings.SplitN(commit.Message, "\n", 2)
		firstLine := messageLines[0]
		if firstLine == "" {
			firstLine = "(no message)"
		}

		shortHash := commit.Hash.String()
		if len(shortHash) > 7 {
			shortHash = shortHash[:7]
		}

		item := &feeds.Item{
			Title:       fmt.Sprintf("[Commit %s] %s", shortHash, firstLine),
			Description: commit.Message,
			Link:        &feeds.Link{Href: fmt.Sprintf("%s/%s/commit/%s", rp.config.Core.BaseUrl(), ownerSlashRepo, commit.Hash.String())},
			Created:     commit.Author.When,
			Author:      &feeds.Author{Name: commit.Author.Name, Email: commit.Author.Email},
		}
		items = append(items, item)
	}

	return items, nil
}

func (rp *Repo) createTagItems(ctx context.Context, repo *models.Repo, ownerSlashRepo string) ([]*feeds.Item, error) {
	xrpcc := rp.knotMirrorXRPCClient()

	tagBytes, err := tangled.GitTempListTags(ctx, xrpcc, "", 100, repo.RepoDid)
	if err != nil {
		return nil, fmt.Errorf("failed to call XRPC repo.tags: %w", err)
	}

	var tagResp types.RepoTagsResponse
	if err := json.Unmarshal(tagBytes, &tagResp); err != nil {
		return nil, fmt.Errorf("failed to decode XRPC response: %w", err)
	}

	var items []*feeds.Item
	for _, tag := range tagResp.Tags {
		var description string

		// only handle annotated tags for now
		if tag.Tag != nil {
			if tag.Tag.Message != "" {
				description = fmt.Sprintf("Tag %s created by %s:\n\n%s", tag.Name, tag.Tag.Tagger.Name, tag.Tag.Message)
			} else {
				description = fmt.Sprintf("Tag %s created by %s", tag.Name, tag.Tag.Tagger.Name)
			}

			item := &feeds.Item{
				Title:       fmt.Sprintf("[Tag] %s", tag.Name),
				Description: description,
				Link:        &feeds.Link{Href: fmt.Sprintf("%s/%s/tags/%s", rp.config.Core.BaseUrl(), ownerSlashRepo, tag.Name)},
				Created:     tag.Tag.Tagger.When,
				Author: &feeds.Author{
					Name:  tag.Tag.Tagger.Name,
					Email: tag.Tag.Tagger.Email,
				},
			}
			items = append(items, item)
		}
	}

	return items, nil
}

func (rp *Repo) getPullState(pull *models.Pull) string {
	if pull.State == models.PullOpen {
		return "opened"
	}
	return pull.State.String()
}

func (rp *Repo) buildPullDescription(handle syntax.Handle, state string, pull *models.Pull, repoName string) string {
	base := fmt.Sprintf("@%s %s pull request #%d", handle, state, pull.PullId)

	if pull.State == models.PullMerged {
		return fmt.Sprintf("%s (on round #%d) in %s", base, pull.LastRoundNumber(), repoName)
	}

	return fmt.Sprintf("%s in %s", base, repoName)
}

func (rp *Repo) AtomFeed(w http.ResponseWriter, r *http.Request) {
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		rp.logger.Error("failed to fully resolve repo", "err", err)
		return
	}
	repoOwnerId, ok := r.Context().Value("resolvedId").(identity.Identity)
	if !ok || repoOwnerId.Handle.IsInvalidHandle() {
		rp.logger.Error("failed to get resolved repo owner id")
		return
	}
	ownerSlashRepo := repoOwnerId.Handle.String() + "/" + f.Slug()

	opts := parseFeedOpts(r)
	feed, err := rp.getRepoFeed(r.Context(), f, ownerSlashRepo, opts)
	if err != nil {
		rp.logger.Error("failed to get repo feed", "err", err)
		rp.pages.Error500(w)
		return
	}

	atom, err := feed.ToAtom()
	if err != nil {
		rp.pages.Error500(w)
		return
	}

	w.Header().Set("content-type", "application/atom+xml")
	w.Write([]byte(atom))
}
