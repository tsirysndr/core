package db

import (
	"database/sql"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type Repo struct {
	Knot      string
	Owner     syntax.DID
	Rkey      syntax.RecordKey
	RepoDid   syntax.DID
	CreatedAt string
}

func (d *DB) AddRepo(repo Repo) error {
	var createdAt sql.NullString
	if repo.CreatedAt != "" {
		createdAt = sql.NullString{String: repo.CreatedAt, Valid: true}
	}
	_, err := d.Exec(
		`insert into repos (knot, owner, rkey, repo_did, created_at)
		 values (?, ?, ?, ?, ?)
		 on conflict(owner, rkey) do update set
		     knot       = excluded.knot,
		     repo_did   = excluded.repo_did,
		     created_at = coalesce(excluded.created_at, repos.created_at)`,
		repo.Knot, repo.Owner.String(), repo.Rkey.String(), repo.RepoDid.String(), createdAt,
	)
	return err
}

func (d *DB) CollapseRepoSiblings(owner, repoDid syntax.DID) (int64, error) {
	res, err := d.Exec(
		`delete from repos
		 where owner = ?
		   and repo_did = ?
		   and (
		     (created_at is null and exists (
		       select 1 from repos r2
		       where r2.owner = repos.owner
		         and r2.repo_did = repos.repo_did
		         and r2.created_at is not null
		         and r2.rkey <> repos.rkey
		     ))
		     or (created_at is not null and created_at < (
		       select max(created_at) from repos
		       where owner = ? and repo_did = ? and created_at is not null
		     ))
		   )`,
		owner.String(), repoDid.String(), owner.String(), repoDid.String(),
	)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

func (d *DB) Knots() ([]string, error) {
	rows, err := d.Query(`select distinct knot from repos`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var knots []string
	for rows.Next() {
		var knot string
		if err := rows.Scan(&knot); err != nil {
			return nil, err
		}
		knots = append(knots, knot)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return knots, nil
}

func scanRepo(row interface{ Scan(...any) error }) (*Repo, error) {
	var knot, owner, rkey, repoDid string
	if err := row.Scan(&knot, &owner, &rkey, &repoDid); err != nil {
		return nil, err
	}
	return &Repo{
		Knot:    knot,
		Owner:   syntax.DID(owner),
		Rkey:    syntax.RecordKey(rkey),
		RepoDid: syntax.DID(repoDid),
	}, nil
}

func (d *DB) GetRepoByDid(repoDid syntax.DID) (*Repo, error) {
	return scanRepo(d.QueryRow(
		`select knot, owner, rkey, coalesce(repo_did, '') from repos where repo_did = ?`,
		repoDid.String(),
	))
}

func (d *DB) GetRepoByOwnerRkey(owner syntax.DID, rkey syntax.RecordKey) (*Repo, error) {
	return scanRepo(d.QueryRow(
		`select knot, owner, rkey, coalesce(repo_did, '') from repos where owner = ? and rkey = ?`,
		owner.String(), rkey.String(),
	))
}

func (d *DB) AllRepos() ([]Repo, error) {
	rows, err := d.Query(`select knot, owner, rkey, coalesce(repo_did, '') from repos`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var repos []Repo
	for rows.Next() {
		r, err := scanRepo(rows)
		if err != nil {
			return nil, err
		}
		repos = append(repos, *r)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return repos, nil
}

func (d *DB) DeleteRepoByOwnerRkey(owner syntax.DID, rkey syntax.RecordKey) error {
	_, err := d.Exec(`delete from repos where owner = ? and rkey = ?`, owner.String(), rkey.String())
	return err
}
