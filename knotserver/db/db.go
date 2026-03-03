package db

import (
	"context"
	"database/sql"
	"fmt"
	"log/slog"
	"os"
	"strings"

	securejoin "github.com/cyphar/filepath-securejoin"
	_ "github.com/mattn/go-sqlite3"
	"tangled.org/core/log"
	"tangled.org/core/orm"
)

type DB struct {
	db     *sql.DB
	logger *slog.Logger
}

func Setup(ctx context.Context, dbPath string) (*DB, error) {
	// https://github.com/mattn/go-sqlite3#connection-string
	opts := []string{
		"_foreign_keys=1",
		"_journal_mode=WAL",
		"_synchronous=NORMAL",
		"_auto_vacuum=incremental",
		"_busy_timeout=5000",
	}

	logger := log.FromContext(ctx)
	logger = log.SubLogger(logger, "db")

	db, err := sql.Open("sqlite3", dbPath+"?"+strings.Join(opts, "&"))
	if err != nil {
		return nil, err
	}

	conn, err := db.Conn(ctx)
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	_, err = conn.ExecContext(ctx, `
		create table if not exists known_dids (
			did text primary key
		);

		create table if not exists public_keys (
			id integer primary key autoincrement,
			did text not null,
			key text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			unique(did, key),
			foreign key (did) references known_dids(did) on delete cascade
		);

		create table if not exists _jetstream (
			id integer primary key autoincrement,
			last_time_us integer not null
		);

		create table if not exists events (
			rkey text not null,
			nsid text not null,
			event text not null, -- json
			created integer not null default (strftime('%s', 'now')),
			primary key (rkey, nsid)
		);

		create table if not exists repo_keys (
			repo_did    text primary key,
			signing_key blob not null,
			created_at  text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		);

		create table if not exists migrations (
			id integer primary key autoincrement,
			name text unique
		);
	`)
	if err != nil {
		return nil, err
	}

	if err := orm.RunMigration(conn, logger, "add-owner-did-to-repo-keys", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `ALTER TABLE repo_keys ADD COLUMN owner_did TEXT`)
		return mErr
	}); err != nil {
		return nil, err
	}

	if err := orm.RunMigration(conn, logger, "add-repo-name-to-repo-keys", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `ALTER TABLE repo_keys ADD COLUMN repo_name TEXT`)
		return mErr
	}); err != nil {
		return nil, err
	}

	if err := orm.RunMigration(conn, logger, "add-unique-owner-repo-on-repo-keys", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `CREATE UNIQUE INDEX IF NOT EXISTS idx_repo_keys_owner_repo ON repo_keys(owner_did, repo_name)`)
		return mErr
	}); err != nil {
		return nil, err
	}

	if err := orm.RunMigration(conn, logger, "add-key-type-and-nullable-signing-key", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `
			create table repo_keys_new (
				repo_did    text primary key,
				signing_key blob,
				created_at  text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				owner_did   text,
				repo_name   text,
				at_uri      text,
				key_type    text not null default 'k256'
			);
			insert into repo_keys_new (repo_did, signing_key, created_at, owner_did, repo_name, key_type)
				select repo_did, signing_key, created_at, owner_did, repo_name, 'k256'
				from repo_keys;
			drop table repo_keys;
			alter table repo_keys_new rename to repo_keys;
			create unique index if not exists idx_repo_keys_owner_repo
				on repo_keys(owner_did, repo_name);
		`)
		return mErr
	}); err != nil {
		return nil, err
	}

	return &DB{
		db:     db,
		logger: logger,
	}, nil
}

func (d *DB) StoreRepoKey(repoDid string, signingKey []byte, ownerDid, repoName, atUri string) error {
	_, err := d.db.Exec(
		`INSERT INTO repo_keys (repo_did, signing_key, owner_did, repo_name, at_uri, key_type) VALUES (?, ?, ?, ?, ?, 'k256')`,
		repoDid, signingKey, ownerDid, repoName, atUri,
	)
	return err
}

func (d *DB) StoreRepoDidWeb(repoDid, ownerDid, repoName, atUri string) error {
	_, err := d.db.Exec(
		`INSERT INTO repo_keys (repo_did, signing_key, owner_did, repo_name, at_uri, key_type) VALUES (?, NULL, ?, ?, ?, 'web')`,
		repoDid, ownerDid, repoName, atUri,
	)
	return err
}

func (d *DB) DeleteRepoKey(repoDid string) error {
	_, err := d.db.Exec(`DELETE FROM repo_keys WHERE repo_did = ?`, repoDid)
	return err
}

func (d *DB) RepoDidExists(repoDid string) (bool, error) {
	var count int
	err := d.db.QueryRow(`SELECT count(1) FROM repo_keys WHERE repo_did = ?`, repoDid).Scan(&count)
	return count > 0, err
}

func (d *DB) GetRepoDid(ownerDid, repoName string) (string, error) {
	var repoDid string
	err := d.db.QueryRow(
		`SELECT repo_did FROM repo_keys WHERE owner_did = ? AND repo_name = ?`,
		ownerDid, repoName,
	).Scan(&repoDid)
	return repoDid, err
}

func (d *DB) GetRepoKeyOwner(repoDid string) (ownerDid string, repoName string, err error) {
	var nullOwner, nullName sql.NullString
	err = d.db.QueryRow(
		`SELECT owner_did, repo_name FROM repo_keys WHERE repo_did = ?`,
		repoDid,
	).Scan(&nullOwner, &nullName)
	if err != nil {
		return
	}
	if !nullOwner.Valid || !nullName.Valid || nullOwner.String == "" || nullName.String == "" {
		err = fmt.Errorf("repo_keys row for %s has empty or null owner_did or repo_name", repoDid)
		return
	}
	ownerDid = nullOwner.String
	repoName = nullName.String
	return
}

func (d *DB) ResolveRepoDIDOnDisk(scanPath, repoDid string) (repoPath, ownerDid, repoName string, err error) {
	ownerDid, repoName, err = d.GetRepoKeyOwner(repoDid)
	if err != nil {
		return
	}

	didPath, joinErr := securejoin.SecureJoin(scanPath, repoDid)
	if joinErr != nil {
		err = fmt.Errorf("securejoin failed for repo DID path %s: %w", repoDid, joinErr)
		return
	}

	if _, statErr := os.Stat(didPath); statErr != nil {
		err = fmt.Errorf("repo DID directory not found on disk: %s", didPath)
		return
	}

	repoPath = didPath
	return
}
