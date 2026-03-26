package db

import (
	"context"
	"database/sql"
	"log/slog"
	"strings"

	_ "github.com/mattn/go-sqlite3"
	"tangled.org/core/log"
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

		create table if not exists migrations (
			id integer primary key autoincrement,
			name text unique
		);
	`)
	if err != nil {
		return nil, err
	}

	return &DB{
		db:     db,
		logger: logger,
	}, nil
}
