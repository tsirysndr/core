package db

import (
	"context"
	"database/sql"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/orm"
)

type Collaborator struct {
	Id      int
	RepoDid syntax.DID
	Subject syntax.DID
	AddedBy syntax.DID
	Created string
}

func AddCollaborator(q DBTX, c Collaborator) error {
	_, err := q.Exec(
		`insert or ignore into collaborators (repo_did, subject_did, added_by_did) values (?, ?, ?)`,
		c.RepoDid,
		c.Subject,
		c.AddedBy,
	)
	return err
}

func IsCollaborator(q DBTX, repoDid, subject syntax.DID) (bool, error) {
	var exists bool
	err := q.QueryRow(
		`select exists (select 1 from collaborators where repo_did = ? and subject_did = ?)`,
		repoDid,
		subject,
	).Scan(&exists)
	return exists, err
}

func RemoveCollaborator(q DBTX, repoDid, subject syntax.DID) error {
	_, err := q.Exec(
		`delete from collaborators where repo_did = ? and subject_did = ?`,
		repoDid,
		subject,
	)
	return err
}

func (d *DB) ApplyCollaboratorBackfill(ctx context.Context, rows []Collaborator, migrationName string, markApplied bool) error {
	insert := func(tx *sql.Tx) error {
		for _, c := range rows {
			if err := AddDid(tx, c.Subject.String()); err != nil {
				return err
			}
			if err := AddCollaborator(tx, c); err != nil {
				return err
			}
		}
		return nil
	}

	if markApplied {
		conn, err := d.db.Conn(ctx)
		if err != nil {
			return err
		}
		defer conn.Close()
		return orm.RunMigration(conn, d.logger, migrationName, insert)
	}

	tx, err := d.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := insert(tx); err != nil {
		return err
	}
	return tx.Commit()
}

func ListCollaborators(q DBTX, repoDid syntax.DID, p ListPage) ([]Collaborator, *int, error) {
	return listPaged(q,
		`select id, repo_did, subject_did, added_by_did, created
		from collaborators
		where repo_did = ?`,
		[]any{repoDid}, p,
		func(r *sql.Rows) (Collaborator, error) {
			var c Collaborator
			err := r.Scan(&c.Id, &c.RepoDid, &c.Subject, &c.AddedBy, &c.Created)
			return c, err
		},
		func(c Collaborator) int { return c.Id },
	)
}
