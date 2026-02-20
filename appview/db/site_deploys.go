package db

import (
	"fmt"
	"time"

	"tangled.org/core/appview/models"
)

// AddSiteDeploy records a site deploy attempt.
func AddSiteDeploy(e Execer, deploy *models.SiteDeploy) error {
	result, err := e.Exec(`
		insert into site_deploys (
			repo_at,
			branch,
			dir,
			commit_sha,
			status,
			trigger,
			error
		) values (?, ?, ?, ?, ?, ?, ?)
	`,
		deploy.RepoAt,
		deploy.Branch,
		deploy.Dir,
		deploy.CommitSHA,
		string(deploy.Status),
		string(deploy.Trigger),
		deploy.Error,
	)
	if err != nil {
		return fmt.Errorf("failed to insert site deploy: %w", err)
	}

	id, err := result.LastInsertId()
	if err != nil {
		return fmt.Errorf("failed to get site deploy id: %w", err)
	}

	deploy.Id = id
	return nil
}

// GetSiteDeploys returns recent deploy records for a repository, newest first.
func GetSiteDeploys(e Execer, repoAt string, limit int) ([]models.SiteDeploy, error) {
	if limit <= 0 {
		limit = 20
	}

	rows, err := e.Query(`
		select
			id,
			repo_at,
			branch,
			dir,
			commit_sha,
			status,
			trigger,
			error,
			created_at
		from site_deploys
		where repo_at = ?
		order by created_at desc
		limit ?
	`, repoAt, limit)
	if err != nil {
		return nil, fmt.Errorf("failed to query site deploys: %w", err)
	}
	defer rows.Close()

	var deploys []models.SiteDeploy
	for rows.Next() {
		var d models.SiteDeploy
		var createdAt string

		if err := rows.Scan(
			&d.Id,
			&d.RepoAt,
			&d.Branch,
			&d.Dir,
			&d.CommitSHA,
			&d.Status,
			&d.Trigger,
			&d.Error,
			&createdAt,
		); err != nil {
			return nil, fmt.Errorf("failed to scan site deploy: %w", err)
		}

		if t, err := time.Parse(time.RFC3339, createdAt); err == nil {
			d.CreatedAt = t
		}

		deploys = append(deploys, d)
	}

	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to iterate site deploys: %w", err)
	}

	return deploys, nil
}
