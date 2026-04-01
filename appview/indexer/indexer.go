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
	Db     *db.DB
	notify.BaseNotifier
}

func New(logger *slog.Logger, db *db.DB) *Indexer {
	return &Indexer{
		issues_indexer.NewIndexer("indexes/issues.bleve"),
		pulls_indexer.NewIndexer("indexes/pulls.bleve"),
		repos_indexer.NewIndexer("indexes/repos.bleve"),
		logger,
		db,
		notify.BaseNotifier{},
	}
}

// Init initializes all indexers
func (ix *Indexer) Init(ctx context.Context) error {
	ctx = tlog.IntoContext(ctx, ix.logger)
	ix.Issues.Init(ctx, ix.Db)
	ix.Pulls.Init(ctx, ix.Db)
	ix.Repos.Init(ctx, ix.Db)
	return nil
}
