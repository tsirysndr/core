package indexer

import (
	"context"
	"log/slog"

	"tangled.org/core/appview/db"
	issues_indexer "tangled.org/core/appview/indexer/issues"
	pulls_indexer "tangled.org/core/appview/indexer/pulls"
	repos_indexer "tangled.org/core/appview/indexer/repos"
	"tangled.org/core/appview/notify"
	tlog "tangled.org/core/log"
)

type Indexer struct {
	Issues *issues_indexer.Indexer
	Pulls  *pulls_indexer.Indexer
	Repos  *repos_indexer.Indexer
	logger *slog.Logger
	notify.BaseNotifier
}

func New(logger *slog.Logger) *Indexer {
	return &Indexer{
		issues_indexer.NewIndexer("indexes/issues.bleve"),
		pulls_indexer.NewIndexer("indexes/pulls.bleve"),
		repos_indexer.NewIndexer("indexes/repos.bleve"),
		logger,
		notify.BaseNotifier{},
	}
}

// Init initializes all indexers
func (ix *Indexer) Init(ctx context.Context, db *db.DB) error {
	ctx = tlog.IntoContext(ctx, ix.logger)
	ix.Issues.Init(ctx, db)
	ix.Pulls.Init(ctx, db)
	ix.Repos.Init(ctx, db)
	return nil
}
