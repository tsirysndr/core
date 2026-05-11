package db

import (
	"database/sql"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type RepoCollaborator struct {
	OwnerDid syntax.DID
	Rkey     syntax.RecordKey
	Subject  syntax.DID
	RepoDid  syntax.DID
}

func (d *DB) AddRepoCollaborator(c RepoCollaborator) error {
	_, err := d.Exec(
		`insert into repo_collaborators (owner_did, rkey, subject, repo_did)
		 values (?, ?, ?, ?)
		 on conflict(owner_did, rkey) do update set
		     subject  = excluded.subject,
		     repo_did = excluded.repo_did`,
		c.OwnerDid.String(), c.Rkey.String(), c.Subject.String(), c.RepoDid.String(),
	)
	return err
}

func scanCollab(row interface{ Scan(...any) error }) (*RepoCollaborator, error) {
	var owner, rkey, subject, repoDid string
	if err := row.Scan(&owner, &rkey, &subject, &repoDid); err != nil {
		return nil, err
	}
	return &RepoCollaborator{
		OwnerDid: syntax.DID(owner),
		Rkey:     syntax.RecordKey(rkey),
		Subject:  syntax.DID(subject),
		RepoDid:  syntax.DID(repoDid),
	}, nil
}

func (d *DB) GetRepoCollaborator(ownerDid syntax.DID, rkey syntax.RecordKey) (*RepoCollaborator, error) {
	return scanCollab(d.QueryRow(
		`select owner_did, rkey, subject, repo_did from repo_collaborators where owner_did = ? and rkey = ?`,
		ownerDid.String(), rkey.String(),
	))
}

func (d *DB) DeleteRepoCollaborator(ownerDid syntax.DID, rkey syntax.RecordKey) error {
	res, err := d.Exec(`delete from repo_collaborators where owner_did = ? and rkey = ?`, ownerDid.String(), rkey.String())
	if err != nil {
		return err
	}
	n, err := res.RowsAffected()
	if err != nil {
		return err
	}
	if n == 0 {
		return sql.ErrNoRows
	}
	return nil
}

func (d *DB) DeleteRepoCollaboratorsByRepoDid(repoDid syntax.DID) error {
	_, err := d.Exec(`delete from repo_collaborators where repo_did = ?`, repoDid.String())
	if err != nil {
		return fmt.Errorf("delete collaborators for %s: %w", repoDid, err)
	}
	return nil
}

func (d *DB) ListCollaboratorsByRepoDid(repoDid syntax.DID) ([]RepoCollaborator, error) {
	rows, err := d.Query(
		`select owner_did, rkey, subject, repo_did from repo_collaborators where repo_did = ?`,
		repoDid.String(),
	)
	if err != nil {
		return nil, fmt.Errorf("list collaborators for %s: %w", repoDid, err)
	}
	defer rows.Close()

	var out []RepoCollaborator
	for rows.Next() {
		c, err := scanCollab(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, *c)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return out, nil
}
