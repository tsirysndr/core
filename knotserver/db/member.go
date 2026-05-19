package db

import (
	"context"
	"database/sql"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/orm"
)

type KnotMember struct {
	Id      int
	Did     syntax.DID
	Rkey    string
	Subject syntax.DID
}

func (d *DB) IsMigrationApplied(name string) (bool, error) {
	var exists bool
	err := d.db.QueryRow(
		`select exists (select 1 from migrations where name = ?)`,
		name,
	).Scan(&exists)
	return exists, err
}

func (d *DB) ApplyKnotMembersBackfill(ctx context.Context, rows []KnotMember, migrationName string) error {
	conn, err := d.db.Conn(ctx)
	if err != nil {
		return err
	}
	defer conn.Close()

	return orm.RunMigration(conn, d.logger, migrationName, func(tx *sql.Tx) error {
		for _, m := range rows {
			if _, err := tx.ExecContext(ctx,
				`insert or ignore into known_dids (did) values (?)`,
				m.Subject,
			); err != nil {
				return err
			}
			if _, err := tx.ExecContext(ctx,
				`insert or ignore into knot_members (did, rkey, subject) values (?, ?, ?)`,
				m.Did, m.Rkey, m.Subject,
			); err != nil {
				return err
			}
		}
		return nil
	})
}

func AddKnotMember(q DBTX, member KnotMember) error {
	_, err := q.Exec(
		`insert or ignore into knot_members (did, rkey, subject) values (?, ?, ?)`,
		member.Did,
		member.Rkey,
		member.Subject,
	)
	return err
}

func RemoveKnotMember(q DBTX, ownerDid, rkey string) error {
	_, err := q.Exec(
		"delete from knot_members where did = ? and rkey = ?",
		ownerDid,
		rkey,
	)
	return err
}

func CountKnotMembersBySubject(q DBTX, subject string) (int, error) {
	var count int
	err := q.QueryRow(
		`select count(*) from knot_members where subject = ?`,
		subject,
	).Scan(&count)
	return count, err
}

func GetKnotMember(q DBTX, did, rkey string) (*KnotMember, error) {
	query :=
		`select id, did, rkey, subject
		from knot_members
		where did = ? and rkey = ?`

	var member KnotMember
	err := q.QueryRow(query, did, rkey).Scan(
		&member.Id,
		&member.Did,
		&member.Rkey,
		&member.Subject,
	)
	if err != nil {
		return nil, err
	}

	return &member, nil
}
