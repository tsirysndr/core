package db

import (
	"context"
	"database/sql"
	"fmt"
	"time"

	_ "github.com/jackc/pgx/v5/stdlib"
)

func Make(ctx context.Context, dbUrl string, maxConns int) (*sql.DB, error) {
	db, err := sql.Open("pgx", dbUrl)
	if err != nil {
		return nil, fmt.Errorf("opening db: %w", err)
	}

	db.SetMaxOpenConns(maxConns)
	db.SetMaxIdleConns(maxConns)
	db.SetConnMaxIdleTime(time.Hour)

	pingCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	if err := db.PingContext(pingCtx); err != nil {
		db.Close()
		return nil, fmt.Errorf("ping db: %w", err)
	}

	conn, err := db.Conn(ctx)
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	_, err = conn.ExecContext(ctx, `
		create table if not exists repos (
			did text not null,
			rkey text not null,
			at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.repo' || '/' || rkey) stored,
			cid text not null,

			-- record content
			name text not null,
			knot_domain text not null,

			-- sync data
			git_rev text not null,
			repo_sha text not null,
			state text not null default 'pending',
			error_msg text,
			retry_count integer not null default 0,
			retry_after integer not null default 0,
			db_created_at timestamptz not null default now(),
			db_updated_at timestamptz not null default now(),

			constraint repos_pkey primary key (did, rkey)
		);

		-- knot hosts
		create table if not exists hosts (
			hostname text not null,
			no_ssl boolean not null default false,
			status text not null default 'active',
			last_seq bigint not null default -1,
			db_created_at timestamptz not null default now(),
			db_updated_at timestamptz not null default now(),

			constraint hosts_pkey primary key (hostname)
		);

		create index if not exists idx_repos_aturi on repos (at_uri);
		create index if not exists idx_repos_db_updated_at on repos (db_updated_at desc);
		create index if not exists idx_hosts_db_updated_at on hosts (db_updated_at desc);

		create or replace function set_updated_at()
		returns trigger as $$
		begin
			new.db_updated_at = now();
			return new;
		end;
		$$ language plpgsql;

		drop trigger if exists repos_set_updated_at on repos;
		create trigger repos_set_updated_at
		before update on repos
		for each row
		execute function set_updated_at();

		drop trigger if exists hosts_set_updated_at on hosts;
		create trigger hosts_set_updated_at
		before update on hosts
		for each row
		execute function set_updated_at();
	`)
	if err != nil {
		return nil, fmt.Errorf("initializing db schema: %w", err)
	}

	return db, nil
}
