package db

import (
	"database/sql"
)

// GetOrAssignOwnerUID returns the virtual UID for ownerDID, minting a new one
// from the uid_counter table if this owner has not been seen before.
// UIDs start at 100000 and increment by one per unique owner.
//
// A process-wide mutex serialises concurrent callers so two simultaneous
// requests for distinct DIDs do not race to claim the same counter value.
// The mutex is local to this DB instance and SQLite itself only permits one
// writer at a time, so this does not impede throughput in practice.
func (d *DB) GetOrAssignOwnerUID(ownerDID string) (uint32, error) {
	d.uidAssignMu.Lock()
	defer d.uidAssignMu.Unlock()

	tx, err := d.db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()

	var uid uint32
	err = tx.QueryRow(
		`SELECT uid FROM owner_uid_assignments WHERE owner_did = ?`,
		ownerDID,
	).Scan(&uid)
	if err == nil {
		return uid, tx.Commit()
	}
	if err != sql.ErrNoRows {
		return 0, err
	}

	if err := tx.QueryRow(`SELECT next_uid FROM uid_counter`).Scan(&uid); err != nil {
		return 0, err
	}
	if _, err := tx.Exec(`UPDATE uid_counter SET next_uid = next_uid + 1`); err != nil {
		return 0, err
	}
	if _, err := tx.Exec(
		`INSERT INTO owner_uid_assignments (owner_did, uid) VALUES (?, ?)`,
		ownerDID, uid,
	); err != nil {
		return 0, err
	}

	return uid, tx.Commit()
}

// AllReposForMigration returns all (repo_did, owner_did) pairs with a
// non-null owner. Pass force=true to include already-migrated repos.
func (d *DB) AllReposForMigration(force bool) ([]RepoMigrationRow, error) {
	query := `SELECT repo_did, owner_did FROM repo_keys WHERE owner_did IS NOT NULL`
	if !force {
		query += ` AND isolated_at IS NULL`
	}
	rows, err := d.db.Query(query)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var result []RepoMigrationRow
	for rows.Next() {
		var r RepoMigrationRow
		if err := rows.Scan(&r.RepoDID, &r.OwnerDID); err != nil {
			return nil, err
		}
		result = append(result, r)
	}
	return result, rows.Err()
}

// CountUnmigratedRepos returns the number of repos that have not yet been
// isolation-migrated.
func (d *DB) CountUnmigratedRepos() (int, error) {
	var n int
	err := d.db.QueryRow(`
		SELECT count(1) FROM repo_keys
		WHERE owner_did IS NOT NULL
		  AND isolated_at IS NULL
	`).Scan(&n)
	return n, err
}

// MarkRepoIsolated sets isolated_at to the current time for repoDID.
func (d *DB) MarkRepoIsolated(repoDID string) error {
	_, err := d.db.Exec(
		`UPDATE repo_keys SET isolated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE repo_did = ?`,
		repoDID,
	)
	return err
}

// RepoMigrationRow is a row returned by AllReposForMigration.
type RepoMigrationRow struct {
	RepoDID  string
	OwnerDID string
}
