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

type Querier interface {
	QueryRow(query string, args ...any) *sql.Row
	Exec(query string, args ...any) (sql.Result, error)
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

	if err := orm.RunMigration(conn, logger, "add-repo-aliases", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `
			create table if not exists repo_aliases (
				owner_did text not null,
				rkey      text not null,
				repo_did  text not null,
				rev       text not null,
				primary key (owner_did, rkey)
			);
			create index if not exists idx_repo_aliases_repo_did on repo_aliases(repo_did);

			insert or ignore into repo_aliases (owner_did, rkey, repo_did, rev)
			select owner_did, repo_name, repo_did, '1_' || created_at
			from repo_keys
			where owner_did is not null and repo_name is not null and repo_did is not null;
		`)
		return mErr
	}); err != nil {
		return nil, err
	}

	if err := orm.RunMigration(conn, logger, "drop-at-uri-from-repo-keys", func(tx *sql.Tx) error {
		_, mErr := tx.ExecContext(ctx, `
			create table repo_keys_new (
				repo_did    text primary key,
				signing_key blob,
				created_at  text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				owner_did   text,
				repo_name   text,
				key_type    text not null default 'k256'
			);
			insert into repo_keys_new (repo_did, signing_key, created_at, owner_did, repo_name, key_type)
				select repo_did, signing_key, created_at, owner_did, repo_name, key_type
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

func (d *DB) StoreRepoKey(repoDid string, signingKey []byte, ownerDid, repoName string) error {
	return d.storeRepoKeyRow(repoDid, signingKey, ownerDid, repoName, "k256")
}

func (d *DB) StoreRepoDidWeb(repoDid, ownerDid, repoName string) error {
	return d.storeRepoKeyRow(repoDid, nil, ownerDid, repoName, "web")
}

func (d *DB) storeRepoKeyRow(repoDid string, signingKey []byte, ownerDid, repoName, keyType string) error {
	tx, err := d.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if _, err := tx.Exec(
		`INSERT INTO repo_keys (repo_did, signing_key, owner_did, repo_name, key_type) VALUES (?, ?, ?, ?, ?)`,
		repoDid, signingKey, ownerDid, repoName, keyType,
	); err != nil {
		return err
	}

	if _, err := tx.Exec(
		`INSERT INTO repo_aliases (owner_did, rkey, repo_did, rev)
		 VALUES (?, ?, ?, '0_' || strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		 ON CONFLICT(owner_did, rkey) DO NOTHING`,
		ownerDid, repoName, repoDid,
	); err != nil {
		return err
	}

	return tx.Commit()
}

func (d *DB) DeleteRepoKey(repoDid string) error {
	tx, err := d.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if _, err := tx.Exec(`DELETE FROM repo_aliases WHERE repo_did = ?`, repoDid); err != nil {
		return err
	}

	if _, err := tx.Exec(`DELETE FROM repo_keys WHERE repo_did = ?`, repoDid); err != nil {
		return err
	}

	return tx.Commit()
}

func (d *DB) RepoDidExists(repoDid string) (bool, error) {
	var count int
	err := d.db.QueryRow(`SELECT count(1) FROM repo_keys WHERE repo_did = ?`, repoDid).Scan(&count)
	return count > 0, err
}

func (d *DB) GetRepoDid(ownerDid, rkey string) (string, error) {
	var repoDid string
	err := d.db.QueryRow(
		`SELECT repo_did FROM repo_aliases WHERE owner_did = ? AND rkey = ?`,
		ownerDid, rkey,
	).Scan(&repoDid)
	return repoDid, err
}

func (d *DB) GetRepoDidByName(ownerDid, repoName string) (string, error) {
	var repoDid string
	err := d.db.QueryRow(
		`SELECT repo_did FROM repo_keys WHERE owner_did = ? AND repo_name = ?`,
		ownerDid, repoName,
	).Scan(&repoDid)
	return repoDid, err
}

func (d *DB) GetRepoKeyOwner(repoDid string) (string, string, error) {
	return GetRepoKeyOwner(d.db, repoDid)
}

func GetRepoKeyOwner(q Querier, repoDid string) (ownerDid string, repoName string, err error) {
	err = q.QueryRow(
		`SELECT owner_did, rkey FROM repo_aliases
		 WHERE repo_did = ?
		 ORDER BY rev DESC
		 LIMIT 1`,
		repoDid,
	).Scan(&ownerDid, &repoName)
	if err != nil {
		return
	}
	if ownerDid == "" || repoName == "" {
		err = fmt.Errorf("repo_aliases row for %s has empty owner_did or rkey", repoDid)
		return
	}
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
