package db

import (
	"context"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/models"
)

const EntityStateBackfillName = "backfill-entity-state"

type BackfillSubject struct {
	Subject   syntax.ATURI
	Value     models.StateValue
	CreatedAt string
}

func EnqueueEntityStateBackfill(ctx context.Context, e Execer) (int64, error) {
	res, err := e.ExecContext(ctx, `
		insert into pds_migration (name, did, collection, rkey)
		select distinct ?, r.did, '', ''
		from repos r
		where r.did like 'did:%'
		and (
			exists (
				select 1 from issues i
				where i.repo_did = r.repo_did and i.open = 0 and i.deleted is null and i.rkey != ''
				and not exists (select 1 from issue_states s where s.subject = i.at_uri)
			)
			or exists (
				select 1 from pulls p
				where p.repo_did = r.repo_did and p.state in (0, 2) and p.rkey != ''
				and not exists (select 1 from pull_states s where s.subject = p.at_uri)
			)
		)
		on conflict(name, did, collection, rkey) do update set
			status = 'pending',
			retry_count = 0,
			retry_after = 0,
			error_msg = null
		where pds_migration.status in ('done', 'failed')
	`, EntityStateBackfillName)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

func ColumnOnlyClosedSubjectsForOwner(ctx context.Context, e Execer, owner syntax.DID) ([]BackfillSubject, error) {
	rows, err := e.QueryContext(ctx, `
		select i.at_uri, ?, i.created
		from issues i join repos r on i.repo_did = r.repo_did
		where r.did = ? and i.open = 0 and i.deleted is null and i.rkey != ''
		and not exists (select 1 from issue_states s where s.subject = i.at_uri)
		union all
		select p.at_uri, case p.state when 2 then ? else ? end, p.created
		from pulls p join repos r on p.repo_did = r.repo_did
		where r.did = ? and p.state in (0, 2) and p.rkey != ''
		and not exists (select 1 from pull_states s where s.subject = p.at_uri)
	`,
		string(models.StateClosed),
		owner,
		string(models.StateMerged), string(models.StateClosed),
		owner,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var subjects []BackfillSubject
	for rows.Next() {
		var subject, value, created string
		if err := rows.Scan(&subject, &value, &created); err != nil {
			return nil, err
		}
		subjects = append(subjects, BackfillSubject{
			Subject:   syntax.ATURI(subject),
			Value:     models.StateValue(value),
			CreatedAt: created,
		})
	}
	return subjects, rows.Err()
}
