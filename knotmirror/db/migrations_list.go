package db

import (
	"context"
	"database/sql"
	"fmt"

	"tangled.org/core/log"
)

var Migrations = []Migration{
	{
		Name: "repos_pk_to_repo_did",
		Fn:   reposPkToRepoDid,
	},
}

func reposPkToRepoDid(ctx context.Context, tx *sql.Tx) error {
	if _, err := tx.ExecContext(ctx,
		`alter table repos add column if not exists repo_did text`,
	); err != nil {
		return fmt.Errorf("adding repo_did column: %w", err)
	}
	var bad int
	if err := tx.QueryRowContext(ctx,
		`select count(*) from repos where repo_did is null or repo_did = ''`,
	).Scan(&bad); err != nil {
		return fmt.Errorf("counting rows with null repo_did: %w", err)
	}
	if bad > 0 {
		log.FromContext(ctx).Warn(
			"dropping repos with null repo_did; their on-disk dirs will be orphaned. re-crawl via tap to restore",
			"count", bad,
		)
		if _, err := tx.ExecContext(ctx,
			`delete from repos where repo_did is null or repo_did = ''`,
		); err != nil {
			return fmt.Errorf("deleting null repo_did rows: %w", err)
		}
	}
	return execAll(ctx, tx,
		`alter table repos alter column repo_did set not null`,
		`alter table repos drop constraint if exists repos_pkey`,
		`alter table repos add constraint repos_pkey primary key (repo_did)`,
	)
}

func execAll(ctx context.Context, tx *sql.Tx, stmts ...string) error {
	if len(stmts) == 0 {
		return nil
	}
	if _, err := tx.ExecContext(ctx, stmts[0]); err != nil {
		return err
	}
	return execAll(ctx, tx, stmts[1:]...)
}
