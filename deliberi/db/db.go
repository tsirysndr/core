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
	logger *slog.Logger
}

type Execer interface {
	Query(query string, args ...any) (*sql.Rows, error)
	QueryContext(ctx context.Context, query string, args ...any) (*sql.Rows, error)
	QueryRow(query string, args ...any) *sql.Row
	Exec(query string, args ...any) (sql.Result, error)
}

func Make(ctx context.Context, dbPath string) (*DB, error) {
	opts := []string{
		"_foreign_keys=1",
		"_journal_mode=WAL",
		"_synchronous=NORMAL",
		"_busy_timeout=5000",
	}

	logger := log.SubLogger(log.FromContext(ctx), "db")

	db, err := sql.Open("sqlite3", dbPath+"?"+strings.Join(opts, "&"))
	if err != nil {
		return nil, err
	}

	conn, err := db.Conn(ctx)
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	_, err = conn.ExecContext(ctx, schema)
	if err != nil {
		return nil, err
	}

	if err := runMigrations(conn, logger); err != nil {
		return nil, err
	}

	return &DB{db, logger}, nil
}

func (d *DB) Close() error {
	return d.DB.Close()
}

const schema = `
create table if not exists emails (
	id integer primary key autoincrement,
	did text not null,
	email text not null,
	verified integer not null default 0,
	verification_code text not null,
	last_sent text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
	is_primary integer not null default 0,
	created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
	unique(did, email)
);

create table if not exists signups_inflight (
	id integer primary key autoincrement,
	email text not null unique,
	invite_code text not null,
	created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- one materialized row per (recipient, source record). deliberi owns these;
-- read/emailed are inline. unique(recipient_did, at_uri) dedupes fan-out.
create table if not exists notifications (
	id integer primary key autoincrement,
	recipient_did text not null,
	at_uri text not null,
	type text not null,
	actor_did text not null,
	repo_did text not null default '',
	entity_at text not null default '',
	entity_title text not null default '',
	read integer not null default 0,
	emailed integer not null default 0,
	created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
	unique(recipient_did, at_uri)
);
create index if not exists idx_deliberi_notifs_recipient on notifications(recipient_did, read, created);
create index if not exists idx_deliberi_notifs_digest on notifications(recipient_did, emailed, read, created);

-- small denormalization caches the ingester fills from repo/issue/pull
-- records so notifications render human names without extra lookups.
create table if not exists repo_names (
	repo_did text primary key,
	name text not null,
	owner_did text not null default ''
);
-- repo_did lets a comment find its parent's repo.
create table if not exists entity_titles (
	at_uri text primary key,
	title text not null,
	repo_did text not null default ''
);

create table if not exists jetstream_cursor (
	id integer primary key check (id = 0),
	last_time_us integer not null
);

create table if not exists migrations (
	name text primary key
);

create table if not exists notification_preferences (
	id integer primary key autoincrement,
	user_did text not null unique,
	repo_starred integer not null default 1,
	issue_created integer not null default 1,
	issue_commented integer not null default 1,
	pull_created integer not null default 1,
	pull_commented integer not null default 1,
	followed integer not null default 1,
	pull_merged integer not null default 1,
	issue_closed integer not null default 1,
	user_mentioned integer not null default 1,
	email_notifications integer not null default 0
);
`

func runMigrations(conn *sql.Conn, logger *slog.Logger) error {
	if err := orm.RunMigration(conn, logger, "add-owner-did-to-repo-names", func(tx *sql.Tx) error {
		_, err := tx.Exec(`alter table repo_names add column owner_did text not null default ''`)
		if err != nil && !isColumnExistsErr(err) {
			return err
		}
		return nil
	}); err != nil {
		return err
	}
	return nil
}

func isColumnExistsErr(err error) bool {
	return err != nil && strings.Contains(err.Error(), "duplicate column name")
}
