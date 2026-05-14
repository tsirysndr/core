package db

import (
	"context"
	"database/sql"
	"errors"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/knotmirror/models"
)

func UpsertRepo(ctx context.Context, e *sql.DB, repo *models.Repo) error {
	if repo.RepoDid == "" {
		return fmt.Errorf("upsert repo: repo_did is required")
	}
	if _, err := e.ExecContext(ctx,
		`insert into repos (did, rkey, cid, name, knot_domain, repo_did, git_rev, repo_sha, state, error_msg, retry_count, retry_after)
		values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
		on conflict(repo_did) do update set
			did         = excluded.did,
			rkey        = excluded.rkey,
			cid         = excluded.cid,
			name        = excluded.name,
			knot_domain = excluded.knot_domain,
			git_rev     = excluded.git_rev,
			repo_sha    = excluded.repo_sha,
			state       = excluded.state,
			error_msg   = excluded.error_msg,
			retry_count = excluded.retry_count,
			retry_after = excluded.retry_after`,
		repo.Did,
		repo.Rkey,
		repo.Cid,
		repo.Name,
		repo.KnotDomain,
		repo.RepoDid,
		repo.GitRev,
		repo.RepoSha,
		repo.State,
		repo.ErrorMsg,
		repo.RetryCount,
		repo.RetryAfter,
	); err != nil {
		return fmt.Errorf("upserting repo: %w", err)
	}
	return nil
}

func UpdateRepoState(ctx context.Context, e *sql.DB, repoDid syntax.DID, state models.RepoState) error {
	if _, err := e.ExecContext(ctx,
		`update repos
		set state = $1
		where repo_did = $2`,
		state,
		repoDid,
	); err != nil {
		return fmt.Errorf("updating repo: %w", err)
	}
	return nil
}

func DeleteRepo(ctx context.Context, e *sql.DB, did syntax.DID, rkey syntax.RecordKey) error {
	if _, err := e.ExecContext(ctx,
		`delete from repos where did = $1 and rkey = $2`,
		did,
		rkey,
	); err != nil {
		return fmt.Errorf("deleting repo: %w", err)
	}
	return nil
}

const repoColumns = `
	did,
	rkey,
	cid,
	name,
	knot_domain,
	repo_did,
	git_rev,
	repo_sha,
	state,
	error_msg,
	retry_count,
	retry_after`

func scanRepo(row interface{ Scan(...any) error }) (*models.Repo, error) {
	var repo models.Repo
	if err := row.Scan(
		&repo.Did,
		&repo.Rkey,
		&repo.Cid,
		&repo.Name,
		&repo.KnotDomain,
		&repo.RepoDid,
		&repo.GitRev,
		&repo.RepoSha,
		&repo.State,
		&repo.ErrorMsg,
		&repo.RetryCount,
		&repo.RetryAfter,
	); err != nil {
		return nil, err
	}
	return &repo, nil
}

func GetRepoByRepoDid(ctx context.Context, e *sql.DB, repoDid syntax.DID) (*models.Repo, error) {
	row := e.QueryRowContext(ctx,
		`select`+repoColumns+`
		from repos
		where repo_did = $1`,
		repoDid,
	)
	repo, err := scanRepo(row)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, fmt.Errorf("querying repo: %w", err)
	}
	return repo, nil
}

func GetRepoByAtUri(ctx context.Context, e *sql.DB, aturi syntax.ATURI) (*models.Repo, error) {
	row := e.QueryRowContext(ctx,
		`select`+repoColumns+`
		from repos
		where at_uri = $1`,
		aturi,
	)
	repo, err := scanRepo(row)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, fmt.Errorf("querying repo: %w", err)
	}
	return repo, nil
}

func ListRepos(ctx context.Context, e *sql.DB, page pagination.Page, did, knot, state, name string) ([]models.Repo, error) {
	var conditions []string
	var args []any

	pageClause := ""
	if page.Limit > 0 {
		pageClause = " limit $1 offset $2 "
		args = append(args, page.Limit, page.Offset)
	}

	whereClause := ""
	if did != "" {
		conditions = append(conditions, fmt.Sprintf("did = $%d", len(args)+1))
		args = append(args, did)
	}
	if knot != "" {
		conditions = append(conditions, fmt.Sprintf("knot_domain = $%d", len(args)+1))
		args = append(args, knot)
	}
	if state != "" {
		conditions = append(conditions, fmt.Sprintf("state = $%d", len(args)+1))
		args = append(args, state)
	}
	if name != "" {
		conditions = append(conditions, fmt.Sprintf("name ilike $%d", len(args)+1))
		args = append(args, "%"+name+"%")
	}
	if len(conditions) > 0 {
		whereClause = "WHERE " + conditions[0]
		for _, condition := range conditions[1:] {
			whereClause += " AND " + condition
		}
	}

	query := `
		select` + repoColumns + `
		from repos
	` + whereClause + pageClause
	rows, err := e.QueryContext(ctx, query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var repos []models.Repo
	for rows.Next() {
		repo, err := scanRepo(rows)
		if err != nil {
			return nil, fmt.Errorf("scanning row: %w", err)
		}
		repos = append(repos, *repo)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("scanning rows: %w ", err)
	}

	return repos, nil
}

func GetRepoCountsByState(ctx context.Context, e *sql.DB) (map[models.RepoState]int64, error) {
	const q = `
		SELECT state, COUNT(*)
		FROM repos
		GROUP BY state
	`

	rows, err := e.QueryContext(ctx, q)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	counts := make(map[models.RepoState]int64)

	for rows.Next() {
		var state string
		var count int64

		if err := rows.Scan(&state, &count); err != nil {
			return nil, err
		}

		counts[models.RepoState(state)] = count
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	for _, s := range models.AllRepoStates {
		if _, ok := counts[s]; !ok {
			counts[s] = 0
		}
	}

	return counts, nil
}
