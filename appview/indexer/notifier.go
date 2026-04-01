package indexer

import (
	"context"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/log"
	"tangled.org/core/orm"
)

var _ notify.Notifier = &Indexer{}

func (ix *Indexer) getAndReindexRepo(ctx context.Context, repoAt syntax.ATURI) {
	l := log.FromContext(ctx).With("notifier", "indexer", "repo_at", repoAt)

	repo, err := db.GetRepo(ix.Db, orm.FilterEq("at_uri", repoAt.String()))
	if err != nil {
		l.Error("failed to get repo for reindexing", "err", err)
		return
	}

	err = ix.Repos.Index(ctx, *repo)
	if err != nil {
		l.Error("failed to reindex repo", "err", err)
	}
}

func (ix *Indexer) NewIssue(ctx context.Context, issue *models.Issue, mentions []syntax.DID) {
	l := log.FromContext(ctx).With("notifier", "indexer", "issue", issue)
	l.Debug("indexing new issue")

	err := ix.Issues.Index(ctx, *issue)
	if err != nil {
		l.Error("failed to index an issue", "err", err)
	}

	l.Debug("reindexing repo after new issue")
	ix.getAndReindexRepo(ctx, issue.RepoAt)
}

func (ix *Indexer) NewIssueState(ctx context.Context, actor syntax.DID, issue *models.Issue) {
	l := log.FromContext(ctx).With("notifier", "indexer", "issue", issue)
	l.Debug("updating an issue")
	err := ix.Issues.Index(ctx, *issue)
	if err != nil {
		l.Error("failed to index an issue", "err", err)
	}
}

func (ix *Indexer) DeleteIssue(ctx context.Context, issue *models.Issue) {
	l := log.FromContext(ctx).With("notifier", "indexer", "issue", issue)
	l.Debug("deleting an issue")

	err := ix.Issues.Delete(ctx, issue.Id)
	if err != nil {
		l.Error("failed to delete an issue", "err", err)
	}

	l.Debug("reindexing repo after issue deletion")
	ix.getAndReindexRepo(ctx, issue.RepoAt)
}

func (ix *Indexer) NewIssueLabelOp(ctx context.Context, issue *models.Issue) {
	l := log.FromContext(ctx).With("notifier", "indexer", "issue", issue)
	l.Debug("reindexing issue after label change")
	err := ix.Issues.Index(ctx, *issue)
	if err != nil {
		l.Error("failed to index an issue", "err", err)
	}
}

func (ix *Indexer) NewPullLabelOp(ctx context.Context, pull *models.Pull) {
	l := log.FromContext(ctx).With("notifier", "indexer", "pull", pull)
	l.Debug("reindexing pull after label change")
	err := ix.Pulls.Index(ctx, pull)
	if err != nil {
		l.Error("failed to index a pr", "err", err)
	}
}

func (ix *Indexer) NewPull(ctx context.Context, pull *models.Pull) {
	l := log.FromContext(ctx).With("notifier", "indexer", "pull", pull)
	l.Debug("indexing new pr")

	err := ix.Pulls.Index(ctx, pull)
	if err != nil {
		l.Error("failed to index a pr", "err", err)
	}

	l.Debug("reindexing repo after new pull")
	ix.getAndReindexRepo(ctx, pull.RepoAt)
}

func (ix *Indexer) NewPullState(ctx context.Context, actor syntax.DID, pull *models.Pull) {
	l := log.FromContext(ctx).With("notifier", "indexer", "pull", pull)
	l.Debug("updating a pr")
	err := ix.Pulls.Index(ctx, pull)
	if err != nil {
		l.Error("failed to index a pr", "err", err)
	}
}

func (ix *Indexer) NewRepo(ctx context.Context, repo *models.Repo) {
	l := log.FromContext(ctx).With("notifier", "indexer", "repo", repo)
	l.Debug("indexing new repo")
	err := ix.Repos.Index(ctx, *repo)
	if err != nil {
		l.Error("failed to index a repo", "err", err)
	}
}

func (ix *Indexer) NewStar(ctx context.Context, star *models.Star) {
	l := log.FromContext(ctx).With("notifier", "indexer", "star", star)

	if star.RepoAt.Collection().String() != tangled.RepoNSID {
		return
	}

	l.Debug("reindexing repo after new star")
	ix.getAndReindexRepo(ctx, star.RepoAt)
}

func (ix *Indexer) DeleteStar(ctx context.Context, star *models.Star) {
	l := log.FromContext(ctx).With("notifier", "indexer", "star", star)

	if star.RepoAt.Collection().String() != tangled.RepoNSID {
		return
	}

	l.Debug("reindexing repo after star deletion")
	ix.getAndReindexRepo(ctx, star.RepoAt)
}
