package main

import (
	"context"
	"database/sql"
	"flag"
	"fmt"
	"os"

	_ "github.com/mattn/go-sqlite3"
)

func main() {
	cursorUs := flag.Int64("cursor-us", 0,
		"jetstream cursor (microsecond timestamp) at handoff cutoff. "+
			"The ingester skips events before this. Capture from a running "+
			"deliberi's jetstream_cursor table at snapshot time, or omit to "+
			"let the ingester start from its own current time on first run.")
	flag.Parse()

	if flag.NArg() != 2 {
		fmt.Fprintf(os.Stderr, "usage: %s [--cursor-us <timestamp>] <appview.db> <deliberi.db>\n", os.Args[0])
		os.Exit(1)
	}
	srcPath, dstPath := flag.Arg(0), flag.Arg(1)

	src, err := sql.Open("sqlite3", srcPath+"?_journal_mode=WAL")
	if err != nil {
		fatal("open source: %v", err)
	}
	defer src.Close()

	dst, err := sql.Open("sqlite3", dstPath+"?_foreign_keys=1&_journal_mode=WAL")
	if err != nil {
		fatal("open dest: %v", err)
	}
	defer dst.Close()

	// ATTACH is per-connection in SQLite — pin one connection for everything
	// that touches the destination.
	ctx := context.Background()
	conn, err := dst.Conn(ctx)
	if err != nil {
		fatal("get conn: %v", err)
	}
	defer conn.Close()

	if _, err := conn.ExecContext(ctx, deliberiSchema); err != nil {
		fatal("create schema: %v", err)
	}

	// ATTACH the source DB so we can JOIN across databases.
	if _, err := conn.ExecContext(ctx, "attach database ? as appview", srcPath); err != nil {
		fatal("attach appview: %v", err)
	}

	tx, err := conn.BeginTx(ctx, nil)
	if err != nil {
		fatal("begin tx: %v", err)
	}
	defer tx.Rollback()

	if err := copyEmails(src, tx); err != nil {
		fatal("emails: %v", err)
	}
	fmt.Println("migrated emails")

	if err := copyNotificationPreferences(src, tx); err != nil {
		fatal("notification_preferences: %v", err)
	}
	fmt.Println("migrated notification_preferences")

	if err := copySignupsInflight(src, tx); err != nil {
		fatal("signups_inflight: %v", err)
	}
	fmt.Println("migrated signups_inflight")

	if err := migrateNotifications(tx); err != nil {
		fatal("notifications: %v", err)
	}
	fmt.Println("migrated notifications")

	if err := migrateRepoNames(tx); err != nil {
		fatal("repo_names: %v", err)
	}
	fmt.Println("migrated repo_names")

	if err := migrateEntityTitles(tx); err != nil {
		fatal("entity_titles: %v", err)
	}
	fmt.Println("migrated entity_titles")

	// Optionally seed the jetstream cursor so the ingester skips historical
	// events and doesn't create real-URI duplicates of migrated rows. Capture
	// the cursor from a running instance's jetstream_cursor at handoff time;
	// omit to let the ingester start from its own current time on first run.
	if *cursorUs != 0 {
		if _, err := tx.Exec(
			`insert or replace into jetstream_cursor (id, last_time_us) values (0, ?)`,
			*cursorUs,
		); err != nil {
			fatal("seed cursor: %v", err)
		}
		fmt.Printf("seeded jetstream cursor at %d\n", *cursorUs)
	}

	if err := tx.Commit(); err != nil {
		fatal("commit: %v", err)
	}

	// DETACH is not transactional; run after commit on the same conn.
	if _, err := conn.ExecContext(ctx, "detach database appview"); err != nil {
		fatal("detach: %v", err)
	}
	fmt.Println("migration complete")
}

func copyEmails(src *sql.DB, tx *sql.Tx) error {
	rows, err := src.Query(`select did, email, verified, verification_code, last_sent, is_primary, created from emails`)
	if err != nil {
		return fmt.Errorf("query emails: %w", err)
	}
	defer rows.Close()

	stmt, err := tx.Prepare(`insert into emails (did, email, verified, verification_code, last_sent, is_primary, created) values (?, ?, ?, ?, ?, ?, ?)`)
	if err != nil {
		return fmt.Errorf("prepare: %w", err)
	}
	defer stmt.Close()

	var count int
	for rows.Next() {
		var did, email, code, lastSent, created string
		var verified, isPrimary int
		if err := rows.Scan(&did, &email, &verified, &code, &lastSent, &isPrimary, &created); err != nil {
			return fmt.Errorf("scan: %w", err)
		}
		if _, err := stmt.Exec(did, email, verified, code, lastSent, isPrimary, created); err != nil {
			return fmt.Errorf("insert: %w", err)
		}
		count++
	}
	if err := rows.Err(); err != nil {
		return err
	}
	fmt.Printf("  %d rows\n", count)
	return nil
}

func copyNotificationPreferences(src *sql.DB, tx *sql.Tx) error {
	rows, err := src.Query(`select user_did, repo_starred, issue_created, issue_commented, pull_created, pull_commented, followed, pull_merged, issue_closed, email_notifications from notification_preferences`)
	if err != nil {
		return fmt.Errorf("query prefs: %w", err)
	}
	defer rows.Close()

	stmt, err := tx.Prepare(`insert into notification_preferences (user_did, repo_starred, issue_created, issue_commented, pull_created, pull_commented, followed, pull_merged, issue_closed, email_notifications) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`)
	if err != nil {
		return fmt.Errorf("prepare: %w", err)
	}
	defer stmt.Close()

	var count int
	for rows.Next() {
		var userDid string
		var starred, issueC, issueCm, pullC, pullCm, followed, pullM, issueCl, emailNotif int
		if err := rows.Scan(&userDid, &starred, &issueC, &issueCm, &pullC, &pullCm, &followed, &pullM, &issueCl, &emailNotif); err != nil {
			return fmt.Errorf("scan: %w", err)
		}
		if _, err := stmt.Exec(userDid, starred, issueC, issueCm, pullC, pullCm, followed, pullM, issueCl, emailNotif); err != nil {
			return fmt.Errorf("insert: %w", err)
		}
		count++
	}
	if err := rows.Err(); err != nil {
		return err
	}
	fmt.Printf("  %d rows\n", count)
	return nil
}

func copySignupsInflight(src *sql.DB, tx *sql.Tx) error {
	rows, err := src.Query(`select email, invite_code, created from signups_inflight`)
	if err != nil {
		return fmt.Errorf("query signups: %w", err)
	}
	defer rows.Close()

	stmt, err := tx.Prepare(`insert into signups_inflight (email, invite_code, created) values (?, ?, ?)`)
	if err != nil {
		return fmt.Errorf("prepare: %w", err)
	}
	defer stmt.Close()

	var count int
	for rows.Next() {
		var email, code, created string
		if err := rows.Scan(&email, &code, &created); err != nil {
			return fmt.Errorf("scan: %w", err)
		}
		if _, err := stmt.Exec(email, code, created); err != nil {
			return fmt.Errorf("insert: %w", err)
		}
		count++
	}
	if err := rows.Err(); err != nil {
		return err
	}
	fmt.Printf("  %d rows\n", count)
	return nil
}

func migrateNotifications(tx *sql.Tx) error {
	// old entity_type values: 'issue', 'pull', 'repo', 'follow'
	// old entity_id values:   issue/pull at-uri (for issue/pull), repo DID (for repo), follower DID (for follow)
	//
	// Map to new schema:
	//   at_uri        = deliberi:migrated/{n.id}   (synthetic, unique per row)
	//   repo_did      = repos.repo_did             (the repo identifier, not repos.did)
	//   entity_at     = issue/pull at-uri (from entity_id for issue/pull, or repos.at_uri for star)
	//   entity_title  = issues.title / pulls.title / repos.name
	//   emailed       = 1  (skip digest for migrated rows)

	query := `
		insert into notifications
			(id, recipient_did, at_uri, type, actor_did, repo_did, entity_at, entity_title, read, emailed, created)
		select
			n.id,
			n.recipient_did,
			'deliberi:migrated/' || n.id,
			n.type,
			n.actor_did,
			coalesce(r.repo_did, ''),
			coalesce(
				case
					when n.entity_type in ('issue', 'pull') then n.entity_id
					when n.entity_type = 'repo' then r.at_uri
					else ''
				end,
				''
			),
			coalesce(
				case
					when n.entity_type = 'issue' then i.title
					when n.entity_type = 'pull' then p.title
					when n.entity_type = 'repo' then r.name
					else ''
				end,
				''
			),
			n.read,
			1,
			n.created
		from appview.notifications n
		left join appview.repos r on r.id = n.repo_id
		left join appview.issues i on i.id = n.issue_id
		left join appview.pulls p on p.id = n.pull_id
	`

	res, err := tx.Exec(query)
	if err != nil {
		return fmt.Errorf("migrate notifications: %w", err)
	}
	n, _ := res.RowsAffected()
	fmt.Printf("  %d rows\n", n)
	return nil
}

func migrateRepoNames(tx *sql.Tx) error {
	res, err := tx.Exec(`
		insert or ignore into repo_names (repo_did, name, owner_did)
		select repo_did, name, did from appview.repos
	`)
	if err != nil {
		return fmt.Errorf("migrate repo_names: %w", err)
	}
	n, _ := res.RowsAffected()
	fmt.Printf("  %d rows\n", n)
	return nil
}

func migrateEntityTitles(tx *sql.Tx) error {
	// Populate from both issues and pulls; `union` dedupes overlapping at_uris.
	res, err := tx.Exec(`
		insert or ignore into entity_titles (at_uri, title)
		select at_uri, title from appview.issues
		union
		select at_uri, title from appview.pulls
	`)
	if err != nil {
		return fmt.Errorf("migrate entity_titles: %w", err)
	}
	n, _ := res.RowsAffected()
	fmt.Printf("  %d rows\n", n)
	return nil
}

func fatal(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}

const deliberiSchema = `
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

create table if not exists repo_names (
	repo_did text primary key,
	name text not null,
	owner_did text not null default ''
);
create table if not exists entity_titles (
	at_uri text primary key,
	title text not null
);
create table if not exists jetstream_cursor (
	id integer primary key check (id = 0),
	last_time_us integer not null
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
