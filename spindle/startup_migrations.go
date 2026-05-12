package spindle

import (
	"context"
	"database/sql"
	"fmt"
	"log/slog"

	"tangled.org/core/orm"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/secrets"
)

func runStartupMigrations(ctx context.Context, d *db.DB, vault secrets.Manager, logger *slog.Logger) error {
	conn, err := d.DB.Conn(ctx)
	if err != nil {
		return fmt.Errorf("acquire spindle conn: %w", err)
	}
	defer conn.Close()

	return orm.RunMigration(conn, logger, "copy-owner-rkey-secrets-to-repo-did", func(tx *sql.Tx) error {
		return copyOwnerRkeySecretsToRepoDid(ctx, tx, vault, logger)
	})
}

type repoSecretPair struct {
	oldID, newID secrets.RepoIdentifier
}

func loadRepoSecretPairs(ctx context.Context, tx *sql.Tx) ([]repoSecretPair, error) {
	rows, err := tx.QueryContext(ctx,
		`select owner, rkey, repo_did from repos
		 where repo_did is not null and repo_did <> ''`,
	)
	if err != nil {
		return nil, fmt.Errorf("select repos: %w", err)
	}
	defer rows.Close()

	var collect func(acc []repoSecretPair) ([]repoSecretPair, error)
	collect = func(acc []repoSecretPair) ([]repoSecretPair, error) {
		if !rows.Next() {
			return acc, rows.Err()
		}
		var owner, rkey, repoDid string
		if err := rows.Scan(&owner, &rkey, &repoDid); err != nil {
			return acc, fmt.Errorf("scan repos row: %w", err)
		}
		return collect(append(acc, repoSecretPair{
			oldID: secrets.RepoIdentifier(owner + "/" + rkey),
			newID: secrets.RepoIdentifier(repoDid),
		}))
	}
	return collect(nil)
}

func copyOwnerRkeySecretsToRepoDid(ctx context.Context, tx *sql.Tx, vault secrets.Manager, logger *slog.Logger) error {
	pairs, err := loadRepoSecretPairs(ctx, tx)
	if err != nil {
		return err
	}

	var step func(remaining []repoSecretPair, totalCopied int) error
	step = func(remaining []repoSecretPair, totalCopied int) error {
		if len(remaining) == 0 {
			logger.Info("secret copy migration complete", "rows", len(pairs), "copied", totalCopied)
			return nil
		}
		p := remaining[0]
		n, err := copyRepoSecrets(ctx, vault, p.oldID, p.newID)
		if err != nil {
			return fmt.Errorf("copy %s -> %s: %w", p.oldID, p.newID, err)
		}
		if n > 0 {
			logger.Info("secrets copied", "old", p.oldID, "new", p.newID, "count", n)
		}
		return step(remaining[1:], totalCopied+n)
	}
	return step(pairs, 0)
}
