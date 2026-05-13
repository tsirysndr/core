package db

import (
	"context"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
)

// "migration" for records stored in user's PDS, not AppView DB

// ListPendingPdsRecordMigrations queries list of pending PDS migrations for given user.
// Only pending migrations whose `retry_after` has elapsed are returned.
func ListPendingPdsRecordMigrations(ctx context.Context, e Execer, user syntax.DID) ([]*models.PDSMigration, error) {
	rows, err := e.QueryContext(ctx,
		`with picked as (
			select rowid
			from pds_migration
			where did = ?
				and status = 'pending'
				and retry_after < ?
		)
		update pds_migration
		set status = ?
		where rowid in (select rowid from picked)
		returning name, did, collection, rkey, status, error_msg, retry_count, retry_after`,
		user,
		time.Now().Unix(),
		models.PDSMigrationStatusRunning,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var migrations []*models.PDSMigration
	for rows.Next() {
		var migration models.PDSMigration
		if err := rows.Scan(
			&migration.Name,
			&migration.Did,
			&migration.Collection,
			&migration.Rkey,
			&migration.Status,
			&migration.ErrorMsg,
			&migration.RetryCount,
			&migration.RetryAfter,
		); err != nil {
			return nil, err
		}
		migrations = append(migrations, &migration)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}

	return migrations, nil
}

func HasPendingPdsRecordMigration(ctx context.Context, e Execer, user syntax.DID) (bool, error) {
	var exists bool
	err := e.QueryRowContext(ctx,
		`select exists(
			select 1 from pds_migration
			where did = ?
				and status = 'pending'
				and retry_after < ?
		)`,
		user,
		time.Now().Unix(),
	).Scan(&exists)
	if err != nil {
		return false, err
	}
	return exists, nil
}

func EnqueuePdsRecordMigration(ctx context.Context, e Execer, name string, did syntax.DID, collection syntax.NSID, rkey syntax.RecordKey) error {
	_, err := e.ExecContext(ctx,
		`insert into pds_migration (name, did, collection, rkey)
		values (?, ?, ?, ?)
		on conflict(name, did, collection, rkey) do update set
			status = case when pds_migration.status = 'failed' then 'pending' else pds_migration.status end,
			retry_count = case when pds_migration.status = 'failed' then 0 else pds_migration.retry_count end,
			retry_after = case when pds_migration.status = 'failed' then 0 else pds_migration.retry_after end,
			error_msg = case when pds_migration.status = 'failed' then null else pds_migration.error_msg end`,
		name, did, collection, rkey,
	)
	return err
}

func ReapStaleRunningMigrations(ctx context.Context, e Execer) error {
	_, err := e.ExecContext(ctx,
		`update pds_migration set status = 'pending' where status = 'running'`,
	)
	return err
}

func UpdatePdsRecordMigration(ctx context.Context, e Execer, migration *models.PDSMigration) error {
	_, err := e.ExecContext(ctx,
		`update pds_migration
		set status = ?,
			error_msg = ?,
			retry_count = ?,
			retry_after = ?,
			updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
		where name = ? and did = ? and collection = ? and rkey = ?`,
		migration.Status,
		migration.ErrorMsg,
		migration.RetryCount,
		migration.RetryAfter,
		migration.Name,
		migration.Did,
		migration.Collection,
		migration.Rkey,
	)
	return err
}
