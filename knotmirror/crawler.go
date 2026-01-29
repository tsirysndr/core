package knotmirror

import (
	"context"
	"database/sql"
	"log/slog"

	"tangled.org/core/log"
)

type Crawler struct {
	logger *slog.Logger
	db     *sql.DB
}

func NewCrawler(l *slog.Logger, db *sql.DB) *Crawler {
	return &Crawler{
		logger: log.SubLogger(l, "crawler"),
		db:     db,
	}
}

func (c *Crawler) Start(ctx context.Context) {
	// TODO: repository crawler
}
