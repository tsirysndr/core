package db

import (
	"database/sql"
	"fmt"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func InsertRepoLanguages(e Execer, langs []models.RepoLanguage) error {
	stmt, err := e.Prepare(
		"insert or replace into repo_languages (repo_did, ref, is_default_ref, language, bytes) values (?, ?, ?, ?, ?)",
	)
	if err != nil {
		return err
	}

	for _, l := range langs {
		isDefaultRef := 0
		if l.IsDefaultRef {
			isDefaultRef = 1
		}

		_, err := stmt.Exec(l.RepoDid, l.Ref, isDefaultRef, l.Language, l.Bytes)
		if err != nil {
			return err
		}
	}

	return nil
}

func DeleteRepoLanguages(e Execer, filters ...orm.Filter) error {
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

	query := fmt.Sprintf(`delete from repo_languages %s`, whereClause)

	_, err := e.Exec(query, args...)
	return err
}

func UpdateRepoLanguages(tx *sql.Tx, repoDid syntax.DID, ref string, langs []models.RepoLanguage) error {
	err := DeleteRepoLanguages(
		tx,
		orm.FilterEq("repo_did", repoDid),
		orm.FilterEq("ref", ref),
	)
	if err != nil {
		return fmt.Errorf("failed to delete existing languages: %w", err)
	}

	return InsertRepoLanguages(tx, langs)
}
