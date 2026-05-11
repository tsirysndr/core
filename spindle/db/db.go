package db

import (
	"context"
	"database/sql"
	"log/slog"
	"strings"

	_ "github.com/mattn/go-sqlite3"
	"tangled.org/core/log"
	"tangled.org/core/orm"
)

type DB struct {
	*sql.DB
}

func Make(ctx context.Context, dbPath string) (*DB, error) {
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
		create table if not exists _jetstream (
			id integer primary key autoincrement,
			last_time_us integer not null
		);

		create table if not exists known_dids (
			did text primary key
		);

		create table if not exists repos (
			id         integer primary key autoincrement,
			knot       text not null,
			owner      text not null,
			rkey       text not null,
			repo_did   text,
			created_at text,
			addedAt    text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			unique(owner, rkey)
		);

		create table if not exists repo_collaborators (
			id        integer primary key autoincrement,
			owner_did text not null,
			rkey      text not null,
			subject   text not null,
			repo_did  text not null,
			addedAt   text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			unique(owner_did, rkey)
		);

		create table if not exists spindle_members (
			-- identifiers for the record
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,

			-- data
			instance text not null,
			subject text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique (did, instance, subject)
		);

		-- status event for a single workflow
		create table if not exists events (
			rkey text not null,
			nsid text not null,
			event text not null, -- json
			created integer not null -- unix nanos
		);

		create table if not exists migrations (
			id integer primary key autoincrement,
			name text unique
		);
	`)
	if err != nil {
		return nil, err
	}

	if err := runMigrations(ctx, conn, logger); err != nil {
		return nil, err
	}

	return &DB{db}, nil
}

func runMigrations(_ context.Context, conn *sql.Conn, logger *slog.Logger) error {
	return orm.RunMigration(conn, logger, "repos-to-repo-did", func(tx *sql.Tx) error {
		var hasName int
		if err := tx.QueryRow(
			`select count(*) from pragma_table_info('repos') where name = 'name'`,
		).Scan(&hasName); err != nil {
			return err
		}

		if hasName > 0 {
			var totalRows, copiedRows int
			if err := tx.QueryRow(`select count(*) from repos`).Scan(&totalRows); err != nil {
				return err
			}
			if err := tx.QueryRow(`select count(*) from repos where coalesce(name, '') <> ''`).Scan(&copiedRows); err != nil {
				return err
			}
			if dropped := totalRows - copiedRows; dropped > 0 {
				logger.Warn("dropping repo rows with empty name during migration", "dropped", dropped, "kept", copiedRows)
			}

			if _, err := tx.Exec(`
				create table if not exists repos_new (
					id         integer primary key autoincrement,
					knot       text not null,
					owner      text not null,
					rkey       text not null,
					repo_did   text,
					created_at text,
					addedAt    text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

					unique(owner, rkey)
				);

				insert into repos_new (id, knot, owner, rkey, addedAt)
				select id, knot, owner, name, addedAt from repos where coalesce(name, '') <> '';

				drop table repos;
				alter table repos_new rename to repos;
			`); err != nil {
				return err
			}
		}

		_, err := tx.Exec(`
			create index if not exists idx_repos_repo_did on repos(repo_did);
			create index if not exists idx_repos_owner_repo_did on repos(owner, repo_did);
			create index if not exists idx_repo_collaborators_repo_did
				on repo_collaborators(repo_did);
		`)
		return err
	})
}

func (d *DB) SaveLastTimeUs(lastTimeUs int64) error {
	_, err := d.Exec(`
		insert into _jetstream (id, last_time_us)
		values (1, ?)
		on conflict(id) do update set last_time_us = excluded.last_time_us
	`, lastTimeUs)
	return err
}

func (d *DB) GetLastTimeUs() (int64, error) {
	var lastTimeUs int64
	row := d.QueryRow(`select last_time_us from _jetstream where id = 1;`)
	err := row.Scan(&lastTimeUs)
	return lastTimeUs, err
}
