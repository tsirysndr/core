package db

import (
	"database/sql"
	"errors"
)

type RepoAlias struct {
	OwnerDid string
	Rkey     string
	RepoDid  string
	Rev      string
}

func (d *DB) UpsertRepoAlias(a RepoAlias) error {
	_, err := d.db.Exec(
		`insert into repo_aliases (owner_did, rkey, repo_did, rev)
		 values (?, ?, ?, ?)
		 on conflict(owner_did, rkey) do update set
		     repo_did = excluded.repo_did,
		     rev      = excluded.rev
		 where excluded.rev > repo_aliases.rev`,
		a.OwnerDid, a.Rkey, a.RepoDid, a.Rev,
	)
	return err
}

func (d *DB) DeleteRepoAlias(ownerDid, rkey string) error {
	_, err := d.db.Exec(
		`delete from repo_aliases where owner_did = ? and rkey = ?`,
		ownerDid, rkey,
	)
	return err
}

func (d *DB) ResolveAlias(ownerDid, rkey string) (*RepoAlias, error) {
	var a RepoAlias
	err := d.db.QueryRow(
		`select owner_did, rkey, repo_did, rev from repo_aliases where owner_did = ? and rkey = ?`,
		ownerDid, rkey,
	).Scan(&a.OwnerDid, &a.Rkey, &a.RepoDid, &a.Rev)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &a, nil
}

func (d *DB) CurrentRkey(repoDid string) (ownerDid string, rkey string, err error) {
	err = d.db.QueryRow(
		`select owner_did, rkey from repo_aliases
		 where repo_did = ?
		 order by rev desc
		 limit 1`,
		repoDid,
	).Scan(&ownerDid, &rkey)
	return
}
