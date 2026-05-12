package spindle

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"os"

	_ "github.com/mattn/go-sqlite3"
	"tangled.org/core/spindle/db"
)

const forceTapResyncFlag = "force-tap-repo-resync-v1"

func runStartupMigrations(ctx context.Context, d *db.DB, tapEmbed bool, tapDBPath string, logger *slog.Logger) error {
	if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
		return fmt.Errorf("cleanup orphan repos: %w", err)
	}
	if !tapEmbed {
		logger.Warn("tap not embedded: legacy repos won't auto-resync; trigger external tap resync to migrate secrets/casbin")
		return nil
	}
	if err := nudgeTapForResync(ctx, d, tapDBPath, logger); err != nil {
		return fmt.Errorf("nudge tap for resync: %w", err)
	}
	return nil
}

func cleanupOrphanRepos(ctx context.Context, d *db.DB, logger *slog.Logger) error {
	res, err := d.ExecContext(ctx, `
		delete from repos
		where coalesce(repo_did, '') = ''
		  and exists (
		    select 1 from repos r2
		    where r2.owner = repos.owner
		      and coalesce(r2.repo_did, '') <> ''
		  )
	`)
	if err != nil {
		return fmt.Errorf("delete orphan repos: %w", err)
	}
	n, _ := res.RowsAffected()
	if n > 0 {
		logger.Info("cleaned up orphan repos missing repo_did", "deleted", n)
	}
	return nil
}

func nudgeTapForResync(ctx context.Context, d *db.DB, tapDBPath string, logger *slog.Logger) error {
	if tapDBPath == "" {
		return fmt.Errorf("tap db path empty in embed mode")
	}
	var exists bool
	if err := d.QueryRowContext(ctx,
		`select exists (select 1 from migrations where name = ?)`,
		forceTapResyncFlag,
	).Scan(&exists); err != nil {
		return fmt.Errorf("check %s flag: %w", forceTapResyncFlag, err)
	}
	if exists {
		logger.Warn("skipped migration, already applied", "migration", forceTapResyncFlag)
		return nil
	}

	markDone := func() error {
		if _, err := d.ExecContext(ctx,
			`insert or ignore into migrations (name) values (?)`,
			forceTapResyncFlag,
		); err != nil {
			return fmt.Errorf("mark %s done: %w", forceTapResyncFlag, err)
		}
		return nil
	}

	if _, err := os.Stat(tapDBPath); errors.Is(err, os.ErrNotExist) {
		logger.Info("tap db not yet created, marking resync nudge done", "migration", forceTapResyncFlag, "path", tapDBPath)
		return markDone()
	} else if err != nil {
		return fmt.Errorf("stat tap db: %w", err)
	}

	tdb, err := sql.Open("sqlite3", tapDBPath+"?_busy_timeout=5000")
	if err != nil {
		return fmt.Errorf("open tap db: %w", err)
	}
	defer tdb.Close()

	if _, err := tdb.ExecContext(ctx, `delete from repo_records`); err != nil {
		return fmt.Errorf("clear tap repo_records: %w", err)
	}
	res, err := tdb.ExecContext(ctx,
		`update repos set state = 'desynchronized', retry_after = 0 where state in ('active','error')`,
	)
	if err != nil {
		return fmt.Errorf("desync tap repos: %w", err)
	}
	n, _ := res.RowsAffected()

	if err := markDone(); err != nil {
		return err
	}
	logger.Info("nudged tap to resync", "migration", forceTapResyncFlag, "repos_desynced", n)
	return nil
}
