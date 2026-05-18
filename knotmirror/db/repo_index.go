package db

import (
	"context"
	"database/sql"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
)

func IsLanguageIndexed(ctx context.Context, e *sql.DB, repoId syntax.DID, commitId plumbing.Hash) (bool, error) {
	var exists bool
	err := e.QueryRow(`select exists(select 1 from repo_head_languages where repo = $1)`, repoId).Scan(&exists)
	return exists, err
}

func InsertLanguages(ctx context.Context, e *sql.DB, repoId syntax.DID, commitId plumbing.Hash, langs map[string]int64) error {
	tx, err := e.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("BeginTx: %w", err)
	}
	defer tx.Rollback()

	if _, err := tx.Exec(`delete from repo_head_languages where repo = $1`, repoId); err != nil {
		return fmt.Errorf("deleting old languages: %w", err)
	}

	for lang, size := range langs {
		if _, err := tx.Exec(
			`insert into repo_head_languages (repo, commit, language, size)
			values ($1, $2, $3, $4)`,
			repoId, commitId.String(), lang, size,
		); err != nil {
			return fmt.Errorf("inserting language: %w", err)
		}
	}

	if err := tx.Commit(); err != nil {
		return fmt.Errorf("tx.Commit: %w", err)
	}
	return nil
}

func ListLanguages(ctx context.Context, e *sql.DB, repoId syntax.DID, commitId plumbing.Hash) (map[string]int64, error) {
	sizes := make(map[string]int64)

	rows, err := e.Query(`select language, size from repo_head_languages where repo = $1 and commit = $2`, repoId, commitId.String())
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var lang string
		var size int64
		if err := rows.Scan(&lang, &size); err != nil {
			return nil, err
		}
		sizes[lang] = size
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return sizes, nil
}
