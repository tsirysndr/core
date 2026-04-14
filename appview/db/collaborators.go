package db

import (
	"fmt"
	"strings"
	"time"

	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func AddCollaborator(e Execer, c models.Collaborator) error {
	_, err := e.Exec(
		`insert into collaborators (did, rkey, subject_did, repo_did) values (?, ?, ?, ?);`,
		c.Did, c.Rkey, c.SubjectDid, string(c.RepoDid),
	)
	return err
}

func DeleteCollaborator(e Execer, filters ...orm.Filter) error {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	query := fmt.Sprintf(`delete from collaborators %s`, whereClause)

	_, err := e.Exec(query, args...)
	return err
}

func CollaboratingIn(e Execer, collaborator string) ([]models.Repo, error) {
	rows, err := e.Query(`select repo_did from collaborators where subject_did = ?`, collaborator)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var repoDids []string
	for rows.Next() {
		var repoDid string
		err := rows.Scan(&repoDid)
		if err != nil {
			return nil, err
		}
		repoDids = append(repoDids, repoDid)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	if repoDids == nil {
		return nil, nil
	}

	return GetRepos(e, orm.FilterIn("repo_did", repoDids))
}

func GetCollaborators(e Execer, filters ...orm.Filter) ([]models.Collaborator, error) {
	var collaborators []models.Collaborator
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}
	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}
	query := fmt.Sprintf(`select
			id,
			did,
			rkey,
			subject_did,
			repo_did,
			created
		from collaborators %s`,
		whereClause,
	)
	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	for rows.Next() {
		var collaborator models.Collaborator
		var createdAt string
		if err := rows.Scan(
			&collaborator.Id,
			&collaborator.Did,
			&collaborator.Rkey,
			&collaborator.SubjectDid,
			&collaborator.RepoDid,
			&createdAt,
		); err != nil {
			return nil, err
		}
		collaborator.Created, err = time.Parse(time.RFC3339, createdAt)
		if err != nil {
			collaborator.Created = time.Now()
		}
		collaborators = append(collaborators, collaborator)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return collaborators, nil
}
