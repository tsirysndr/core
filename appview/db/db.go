package db

import (
	"context"
	"database/sql"
	"fmt"
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
	QueryRowContext(ctx context.Context, query string, args ...any) *sql.Row
	Exec(query string, args ...any) (sql.Result, error)
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
	Prepare(query string) (*sql.Stmt, error)
	PrepareContext(ctx context.Context, query string) (*sql.Stmt, error)
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
		create table if not exists registrations (
			id integer primary key autoincrement,
			domain text not null unique,
			did text not null,
			secret text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			registered text
		);
		create table if not exists public_keys (
			id integer primary key autoincrement,
			did text not null,
			name text not null,
			key text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			unique(did, name, key)
		);
		create table if not exists repos (
			id integer primary key autoincrement,
			did text not null,
			name text not null,
			knot text not null,
			rkey text not null,
			at_uri text not null unique,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			unique(did, name, knot, rkey)
		);
		create table if not exists collaborators (
			id integer primary key autoincrement,
			did text not null,
			repo integer not null,
			foreign key (repo) references repos(id) on delete cascade
		);
		create table if not exists follows (
			user_did text not null,
			subject_did text not null,
			rkey text not null,
			followed_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			primary key (user_did, subject_did),
			check (user_did <> subject_did)
		);
		create table if not exists vouches (
			did text not null,
			subject_did text not null,
			cid text not null,
			kind text not null default 'vouch',
			reason text,
			created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			primary key (did, subject_did),
			check (did <> subject_did),
			check (kind in ('vouch', 'denounce'))
		);
		create table if not exists issues (
			id integer primary key autoincrement,
			owner_did text not null,
			repo_at text not null,
			issue_id integer not null,
			title text not null,
			body text not null,
			open integer not null default 1,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			issue_at text,
			unique(repo_at, issue_id),
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);
		create table if not exists comments (
			id integer primary key autoincrement,
			owner_did text not null,
			issue_id integer not null,
			repo_at text not null,
			comment_id integer not null,
			comment_at text not null,
			body text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			unique(issue_id, comment_id),
			foreign key (repo_at, issue_id) references issues(repo_at, issue_id) on delete cascade
		);
		create table if not exists pulls (
			-- identifiers
			id integer primary key autoincrement,
			pull_id integer not null,

			-- at identifiers
			repo_at text not null,
			owner_did text not null,
			rkey text not null,
			pull_at text,

			-- content
			title text not null,
			body text not null,
			target_branch text not null,
			state integer not null default 0 check (state in (0, 1, 2)), -- open, merged, closed

			-- meta
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique(repo_at, pull_id),
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);

		-- every pull must have atleast 1 submission: the initial submission
		create table if not exists pull_submissions (
			-- identifiers
			id integer primary key autoincrement,
			pull_id integer not null,

			-- at identifiers
			repo_at text not null,

			-- content, these are immutable, and require a resubmission to update
			round_number integer not null default 0,
			patch text,

			-- meta
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique(repo_at, pull_id, round_number),
			foreign key (repo_at, pull_id) references pulls(repo_at, pull_id) on delete cascade
		);

		create table if not exists pull_comments (
			-- identifiers
			id integer primary key autoincrement,
			pull_id integer not null,
			submission_id integer not null,

			-- at identifiers
			repo_at text not null,
			owner_did text not null,
			comment_at text not null,

			-- content
			body text not null,

			-- meta
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			foreign key (repo_at, pull_id) references pulls(repo_at, pull_id) on delete cascade,
			foreign key (submission_id) references pull_submissions(id) on delete cascade
		);

		create table if not exists _jetstream (
			id integer primary key autoincrement,
			last_time_us integer not null
		);

		create table if not exists repo_issue_seqs (
			repo_at text primary key,
			next_issue_id integer not null default 1
		);

		create table if not exists repo_pull_seqs (
			repo_at text primary key,
			next_pull_id integer not null default 1
		);

		create table if not exists stars (
			id integer primary key autoincrement,
			starred_by_did text not null,
			repo_at text not null,
			rkey text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			foreign key (repo_at) references repos(at_uri) on delete cascade,
			unique(starred_by_did, repo_at)
		);

		create table if not exists reactions (
			id integer primary key autoincrement,
			reacted_by_did text not null,
			thread_at text not null,
			kind text not null,
			rkey text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			unique(reacted_by_did, thread_at, kind)
		);

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

		create table if not exists artifacts (
			-- id
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,

			-- meta
			repo_at text not null,
			tag binary(20) not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- data
			blob_cid text not null,
			name text not null,
			size integer not null default 0,
			mimetype string not null default "*/*",

			-- constraints
			unique(did, rkey),          -- record must be unique
			unique(repo_at, tag, name), -- for a given tag object, each file must be unique
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);

		create table if not exists profile (
			-- id
			id integer primary key autoincrement,
			did text not null,

			-- data
			description text not null,
			include_bluesky integer not null default 0,
			location text,

			-- constraints
			unique(did)
		);
		create table if not exists profile_links (
			-- id
			id integer primary key autoincrement,
			did text not null,

			-- data
			link text not null,

			-- constraints
			foreign key (did) references profile(did) on delete cascade
		);
		create table if not exists profile_stats (
			-- id
			id integer primary key autoincrement,
			did text not null,

			-- data
			kind text not null check (kind in (
				"merged-pull-request-count",
				"closed-pull-request-count",
				"open-pull-request-count",
				"open-issue-count",
				"closed-issue-count",
				"repository-count"
			)),

			-- constraints
			foreign key (did) references profile(did) on delete cascade
		);
		create table if not exists profile_pinned_repositories (
			-- id
			id integer primary key autoincrement,
			did text not null,

			-- data
			at_uri text not null,

			-- constraints
			unique(did, at_uri),
			foreign key (did) references profile(did) on delete cascade,
			foreign key (at_uri) references repos(at_uri) on delete cascade
		);

		create table if not exists oauth_requests (
			id integer primary key autoincrement,
			auth_server_iss text not null,
			state text not null,
			did text not null,
			handle text not null,
			pds_url text not null,
			pkce_verifier text not null,
			dpop_auth_server_nonce text not null,
			dpop_private_jwk text not null
		);

		create table if not exists oauth_sessions (
			id integer primary key autoincrement,
			did text not null,
			handle text not null,
			pds_url text not null,
			auth_server_iss text not null,
			access_jwt text not null,
			refresh_jwt text not null,
			dpop_pds_nonce text,
			dpop_auth_server_nonce text not null,
			dpop_private_jwk text not null,
			expiry text not null
		);

		create table if not exists punchcard (
			did text not null,
			date text not null, -- yyyy-mm-dd
			count integer,
			primary key (did, date)
		);

		create table if not exists spindles (
			id integer primary key autoincrement,
			owner text not null,
			instance text not null,
			verified text, -- time of verification
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			unique(owner, instance)
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

		create table if not exists pipelines (
			-- identifiers
			id integer primary key autoincrement,
			knot text not null,
			rkey text not null,

			repo_owner text not null,
			repo_name text not null,

			-- every pipeline must be associated with exactly one commit
			sha text not null check (length(sha) = 40),
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- trigger data
			trigger_id integer not null,

			unique(knot, rkey),
			foreign key (trigger_id) references triggers(id) on delete cascade
		);

		create table if not exists triggers (
			-- primary key
			id integer primary key autoincrement,

			-- top-level fields
			kind text not null,

			-- pushTriggerData fields
			push_ref text,
			push_new_sha text check (length(push_new_sha) = 40),
			push_old_sha text check (length(push_old_sha) = 40),

			-- pullRequestTriggerData fields
			pr_source_branch text,
			pr_target_branch text,
			pr_source_sha text check (length(pr_source_sha) = 40),
			pr_action text
		);

		create table if not exists pipeline_statuses (
			-- identifiers
			id integer primary key autoincrement,
			spindle text not null,
			rkey text not null,

			-- referenced pipeline. these form the (did, rkey) pair
			pipeline_knot text not null,
			pipeline_rkey text not null,

			-- content
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			workflow text not null,
			status text not null,
			error text,
			exit_code integer not null default 0,

			unique (spindle, rkey),
			foreign key (pipeline_knot, pipeline_rkey)
				references pipelines (knot, rkey)
				on delete cascade
		);

		create table if not exists repo_languages (
			-- identifiers
			id integer primary key autoincrement,

			-- repo identifiers
			repo_at text not null,
			ref text not null,
			is_default_ref integer not null default 0,

			-- language breakdown
			language text not null,
			bytes integer not null check (bytes >= 0),

			unique(repo_at, ref, language)
		);

		create table if not exists signups_inflight (
			id integer primary key autoincrement,
			email text not null unique,
			invite_code text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		);

		create table if not exists strings (
			-- identifiers
			did text not null,
			rkey text not null,

			-- content
			filename text not null,
			description text,
			content text not null,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			edited text,

			primary key (did, rkey)
		);

		create table if not exists label_definitions (
			-- identifiers
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,
			at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.label.definition' || '/' || rkey) stored,

			-- content
			name text not null,
			value_type text not null check (value_type in (
				"null",
				"boolean",
				"integer",
				"string"
			)),
			value_format text not null default "any",
			value_enum text, -- comma separated list
			scope text not null, -- comma separated list of nsid
			color text,
			multiple integer not null default 0,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique (did, rkey)
			unique (at_uri)
		);

		-- ops are flattened, a record may contain several additions and deletions, but the table will include one row per add/del
		create table if not exists label_ops (
			-- identifiers
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,
			at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.label.op' || '/' || rkey) stored,

			-- content
			subject text not null,
			operation text not null check (operation in ("add", "del")),
			operand_key text not null,
			operand_value text not null,
			-- we need two time values: performed is declared by the user, indexed is calculated by the av
			performed text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			indexed text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			-- traditionally (did, rkey) pair should be unique, but not in this case
			-- operand_key should reference a label definition
			foreign key (operand_key) references label_definitions (at_uri) on delete cascade,
			unique (did, rkey, subject, operand_key, operand_value)
		);

		create table if not exists repo_labels (
			-- identifiers
			id integer primary key autoincrement,

			-- repo identifiers
			repo_at text not null,

			-- label to subscribe to
			label_at text not null,

			unique (repo_at, label_at)
		);

		create table if not exists notifications (
			id integer primary key autoincrement,
			recipient_did text not null,
			actor_did text not null,
			type text not null,
			entity_type text not null,
			entity_id text not null,
			read integer not null default 0,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			repo_id integer references repos(id),
			issue_id integer references issues(id),
			pull_id integer references pulls(id)
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
			email_notifications integer not null default 0
		);

		create table if not exists reference_links (
			id integer primary key autoincrement,
			from_at text not null,
			to_at text not null,
			unique (from_at, to_at)
		);

		create table if not exists webhooks (
			id integer primary key autoincrement,
			repo_at text not null,
			url text not null,
			secret text,
			active integer not null default 1,
			events text not null, -- comma-separated list of events
			created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			updated_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			foreign key (repo_at) references repos(at_uri) on delete cascade
		);

		create table if not exists webhook_deliveries (
			id integer primary key autoincrement,
			webhook_id integer not null,
			event text not null,
			delivery_id text not null,
			url text not null,
			request_body text not null,
			response_code integer,
			response_body text,
			success integer not null default 0,
			created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			foreign key (webhook_id) references webhooks(id) on delete cascade
		);

		create table if not exists bluesky_posts (
			rkey text primary key,
			text text not null,
			created_at text not null,
			langs text,
			facets text,
			embed text,
			like_count integer not null default 0,
			reply_count integer not null default 0,
			repost_count integer not null default 0,
			quote_count integer not null default 0
		);

		create table if not exists domain_claims (
			id integer primary key autoincrement,
			did text not null unique,
			domain text not null unique,
			deleted text -- timestamp when the domain was released/unclaimed; null means actively claimed
		);

		create table if not exists repo_sites (
			id integer primary key autoincrement,
			repo_at text not null unique,
			branch text not null,
			dir text not null default '/',
			is_index integer not null default 0,
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			updated text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);

		create table if not exists site_deploys (
			id integer primary key autoincrement,
			repo_at text not null,
			branch text not null,
			dir text not null default '/',
			commit_sha text not null default '',
			status text not null check (status in ('success', 'failure')),
			trigger text not null check (trigger in ('config_change', 'push')),
			error text not null default '',
			created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);

		create table if not exists punchcard_preferences (
			id integer primary key autoincrement,
			user_did text not null unique,
			hide_mine integer default 0,
			hide_others integer default 0
		);

		create table if not exists newsletter_preferences (
			id         integer primary key autoincrement,
			user_did   text not null unique,
			status     text not null check (status in ('subscribed', 'dismissed')),
			email      text,
			updated_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		);

		create table if not exists vouch_evidences (
			id integer primary key autoincrement,
			vouch_id integer not null,
			at_uri text not null,
			unique(vouch_id, at_uri),
			foreign key (vouch_id) references vouches(id) on delete cascade
		);

		create table if not exists vouch_skips (
			did text not null,
			subject_did text not null,
			created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			primary key (did, subject_did),
			check (did <> subject_did)
		);


		create table if not exists migrations (
			id integer primary key autoincrement,
			name text unique
		);

		-- indexes for better performance
		create index if not exists idx_notifications_recipient_created on notifications(recipient_did, created desc);
		create index if not exists idx_notifications_recipient_read on notifications(recipient_did, read);
		create index if not exists idx_references_from_at on reference_links(from_at);
		create index if not exists idx_references_to_at on reference_links(to_at);
		create index if not exists idx_webhook_deliveries_webhook_id on webhook_deliveries(webhook_id);
		create index if not exists idx_newsletter_prefs_user_did on newsletter_preferences(user_did);
	`)
	if err != nil {
		return nil, err
	}

	// run migrations
	orm.RunMigration(conn, logger, "add-description-to-repos", func(tx *sql.Tx) error {
		tx.Exec(`
			alter table repos add column description text check (length(description) <= 200);
		`)
		return nil
	})

	orm.RunMigration(conn, logger, "add-rkey-to-pubkeys", func(tx *sql.Tx) error {
		// add unconstrained column
		_, err := tx.Exec(`
			alter table public_keys
			add column rkey text;
		`)
		if err != nil {
			return err
		}

		// backfill
		_, err = tx.Exec(`
			update public_keys
			set rkey = ''
			where rkey is null;
		`)
		if err != nil {
			return err
		}

		return nil
	})

	orm.RunMigration(conn, logger, "add-rkey-to-comments", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table comments drop column comment_at;
			alter table comments add column rkey text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-deleted-and-edited-to-issue-comments", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table comments add column deleted text; -- timestamp
			alter table comments add column edited text; -- timestamp
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-source-info-to-pulls-and-submissions", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table pulls add column source_branch text;
			alter table pulls add column source_repo_at text;
			alter table pull_submissions add column source_rev text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-source-to-repos", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table repos add column source text;
		`)
		return err
	})

	// disable foreign-keys for the next migration
	// NOTE: this cannot be done in a transaction, so it is run outside [0]
	//
	// [0]: https://sqlite.org/pragma.html#pragma_foreign_keys
	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "recreate-pulls-column-for-stacking-support", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table pulls_new (
				-- identifiers
				id integer primary key autoincrement,
				pull_id integer not null,

				-- at identifiers
				repo_at text not null,
				owner_did text not null,
				rkey text not null,

				-- content
				title text not null,
				body text not null,
				target_branch text not null,
				state integer not null default 0 check (state in (0, 1, 2, 3)), -- closed, open, merged, deleted

				-- source info
				source_branch text,
				source_repo_at text,

				-- stacking
				stack_id text,
				change_id text,
				parent_change_id text,

				-- meta
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

				-- constraints
				unique(repo_at, pull_id),
				foreign key (repo_at) references repos(at_uri) on delete cascade
			);

			insert into pulls_new (
				id, pull_id,
				repo_at, owner_did, rkey,
				title, body, target_branch, state,
				source_branch, source_repo_at,
				created
			)
			select
				id, pull_id,
				repo_at, owner_did, rkey,
				title, body, target_branch, state,
				source_branch, source_repo_at,
				created
			FROM pulls;

			drop table pulls;
			alter table pulls_new rename to pulls;
		`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	orm.RunMigration(conn, logger, "add-spindle-to-repos", func(tx *sql.Tx) error {
		tx.Exec(`
			alter table repos add column spindle text;
		`)
		return nil
	})

	// drop all knot secrets, add unique constraint to knots
	//
	// knots will henceforth use service auth for signed requests
	orm.RunMigration(conn, logger, "no-more-secrets", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table registrations_new (
				id integer primary key autoincrement,
				domain text not null,
				did text not null,
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				registered text,
				read_only integer not null default 0,
				unique(domain, did)
			);

			insert into registrations_new (id, domain, did, created, registered, read_only)
			select id, domain, did, created, registered, 1 from registrations
			where registered is not null;

			drop table registrations;
			alter table registrations_new rename to registrations;
		`)
		return err
	})

	// recreate and add rkey + created columns with default constraint
	orm.RunMigration(conn, logger, "rework-collaborators-table", func(tx *sql.Tx) error {
		// create new table
		// - repo_at instead of repo integer
		// - rkey field
		// - created field
		_, err := tx.Exec(`
			create table collaborators_new (
				-- identifiers for the record
				id integer primary key autoincrement,
				did text not null,
				rkey text,

				-- content
				subject_did text not null,
				repo_at text not null,

				-- meta
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

				-- constraints
				foreign key (repo_at) references repos(at_uri) on delete cascade
			)
		`)
		if err != nil {
			return err
		}

		// copy data
		_, err = tx.Exec(`
			insert into collaborators_new (id, did, rkey, subject_did, repo_at)
			select
				c.id,
				r.did,
				'',
				c.did,
				r.at_uri
			from collaborators c
			join repos r on c.repo = r.id
		`)
		if err != nil {
			return err
		}

		// drop old table
		_, err = tx.Exec(`drop table collaborators`)
		if err != nil {
			return err
		}

		// rename new table
		_, err = tx.Exec(`alter table collaborators_new rename to collaborators`)
		return err
	})

	orm.RunMigration(conn, logger, "add-rkey-to-issues", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table issues add column rkey text not null default '';

			-- get last url section from issue_at and save to rkey column
			update issues
			set rkey = replace(issue_at, rtrim(issue_at, replace(issue_at, '/', '')), '');
		`)
		return err
	})

	// repurpose the read-only column to "needs-upgrade"
	orm.RunMigration(conn, logger, "rename-registrations-read-only-to-needs-upgrade", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table registrations rename column read_only to needs_upgrade;
		`)
		return err
	})

	// require all knots to upgrade after the release of total xrpc
	orm.RunMigration(conn, logger, "migrate-knots-to-total-xrpc", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			update registrations set needs_upgrade = 1;
		`)
		return err
	})

	// require all knots to upgrade after the release of total xrpc
	orm.RunMigration(conn, logger, "migrate-spindles-to-xrpc-owner", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table spindles add column needs_upgrade integer not null default 0;
		`)
		return err
	})

	// remove issue_at from issues and replace with generated column
	//
	// this requires a full table recreation because stored columns
	// cannot be added via alter
	//
	// couple other changes:
	// - columns renamed to be more consistent
	// - adds edited and deleted fields
	//
	// disable foreign-keys for the next migration
	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "remove-issue-at-from-issues", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists issues_new (
				-- identifiers
				id integer primary key autoincrement,
				did text not null,
				rkey text not null,
				at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.repo.issue' || '/' || rkey) stored,

				-- at identifiers
				repo_at text not null,

				-- content
				issue_id integer not null,
				title text not null,
				body text not null,
				open integer not null default 1,
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				edited text,  -- timestamp
				deleted text,  -- timestamp

				unique(did, rkey),
				unique(repo_at, issue_id),
				unique(at_uri),
				foreign key (repo_at) references repos(at_uri) on delete cascade
			);
		`)
		if err != nil {
			return err
		}

		// transfer data
		_, err = tx.Exec(`
			insert into issues_new (id, did, rkey, repo_at, issue_id, title, body, open, created)
			select
				i.id,
				i.owner_did,
				i.rkey,
				i.repo_at,
				i.issue_id,
				i.title,
				i.body,
				i.open,
				i.created
			from issues i;
		`)
		if err != nil {
			return err
		}

		// drop old table
		_, err = tx.Exec(`drop table issues`)
		if err != nil {
			return err
		}

		// rename new table
		_, err = tx.Exec(`alter table issues_new rename to issues`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	// - renames the comments table to 'issue_comments'
	// - rework issue comments to update constraints:
	//   * unique(did, rkey)
	//   * remove comment-id and just use the global ID
	//   * foreign key (repo_at, issue_id)
	// - new columns
	//   * column "reply_to" which can be any other comment
	//   * column "at-uri" which is a generated column
	orm.RunMigration(conn, logger, "rework-issue-comments", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists issue_comments (
				-- identifiers
				id integer primary key autoincrement,
				did text not null,
				rkey text,
				at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.repo.issue.comment' || '/' || rkey) stored,

				-- at identifiers
				issue_at text not null,
				reply_to text, -- at_uri of parent comment

				-- content
				body text not null,
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				edited text,
				deleted text,

				-- constraints
				unique(did, rkey),
				unique(at_uri),
				foreign key (issue_at) references issues(at_uri) on delete cascade
			);
		`)
		if err != nil {
			return err
		}

		// transfer data
		_, err = tx.Exec(`
			insert into issue_comments (id, did, rkey, issue_at, body, created, edited, deleted)
			select
				c.id,
				c.owner_did,
				c.rkey,
				i.at_uri,  -- get at_uri from issues table
				c.body,
				c.created,
				c.edited,
				c.deleted
			from comments c
			join issues i on c.repo_at = i.repo_at and c.issue_id = i.issue_id;
		`)
		if err != nil {
			return err
		}

		// drop old table
		_, err = tx.Exec(`drop table comments`)
		return err
	})

	// add generated at_uri column to pulls table
	//
	// this requires a full table recreation because stored columns
	// cannot be added via alter
	//
	// disable foreign-keys for the next migration
	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "add-at-uri-to-pulls", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
		create table if not exists pulls_new (
			-- identifiers
			id integer primary key autoincrement,
			pull_id integer not null,
			at_uri text generated always as ('at://' || owner_did || '/' || 'sh.tangled.repo.pull' || '/' || rkey) stored,

			-- at identifiers
			repo_at text not null,
			owner_did text not null,
			rkey text not null,

			-- content
			title text not null,
			body text not null,
			target_branch text not null,
			state integer not null default 0 check (state in (0, 1, 2, 3)), -- closed, open, merged, deleted

			-- source info
			source_branch text,
			source_repo_at text,

			-- stacking
			stack_id text,
			change_id text,
			parent_change_id text,

			-- meta
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique(repo_at, pull_id),
			unique(at_uri),
			foreign key (repo_at) references repos(at_uri) on delete cascade
		);
		`)
		if err != nil {
			return err
		}

		// transfer data
		_, err = tx.Exec(`
		insert into pulls_new (
			id, pull_id, repo_at, owner_did, rkey,
			title, body, target_branch, state,
			source_branch, source_repo_at,
			stack_id, change_id, parent_change_id,
			created
		)
		select
			id, pull_id, repo_at, owner_did, rkey,
			title, body, target_branch, state,
			source_branch, source_repo_at,
			stack_id, change_id, parent_change_id,
			created
			from pulls;
		`)
		if err != nil {
			return err
		}

		// drop old table
		_, err = tx.Exec(`drop table pulls`)
		if err != nil {
			return err
		}

		// rename new table
		_, err = tx.Exec(`alter table pulls_new rename to pulls`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	// remove repo_at and pull_id from pull_submissions and replace with pull_at
	//
	// this requires a full table recreation because stored columns
	// cannot be added via alter
	//
	// disable foreign-keys for the next migration
	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "remove-repo-at-pull-id-from-pull-submissions", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
		create table if not exists pull_submissions_new (
			-- identifiers
			id integer primary key autoincrement,
			pull_at text not null,

			-- content, these are immutable, and require a resubmission to update
			round_number integer not null default 0,
			patch text,
			source_rev text,

			-- meta
			created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

			-- constraints
			unique(pull_at, round_number),
			foreign key (pull_at) references pulls(at_uri) on delete cascade
		);
		`)
		if err != nil {
			return err
		}

		// transfer data, constructing pull_at from pulls table
		_, err = tx.Exec(`
		insert into pull_submissions_new (id, pull_at, round_number, patch, created)
		select
			ps.id,
			'at://' || p.owner_did || '/sh.tangled.repo.pull/' || p.rkey,
			ps.round_number,
			ps.patch,
			ps.created
		from pull_submissions ps
		join pulls p on ps.repo_at = p.repo_at and ps.pull_id = p.pull_id;
		`)
		if err != nil {
			return err
		}

		// drop old table
		_, err = tx.Exec(`drop table pull_submissions`)
		if err != nil {
			return err
		}

		// rename new table
		_, err = tx.Exec(`alter table pull_submissions_new rename to pull_submissions`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	// knots may report the combined patch for a comparison, we can store that on the appview side
	// (but not on the pds record), because calculating the combined patch requires a git index
	orm.RunMigration(conn, logger, "add-combined-column-submissions", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table pull_submissions add column combined text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-pronouns-profile", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table profile add column pronouns text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-meta-column-repos", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table repos add column website text;
			alter table repos add column topics text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-usermentioned-preference", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table notification_preferences add column user_mentioned integer not null default 1;
		`)
		return err
	})

	// remove the foreign key constraints from stars.
	orm.RunMigration(conn, logger, "generalize-stars-subject", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table stars_new (
				id integer primary key autoincrement,
				did text not null,
				rkey text not null,

				subject_at text not null,

				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				unique(did, rkey),
				unique(did, subject_at)
			);

			insert into stars_new (
				id,
				did,
				rkey,
				subject_at,
				created
			)
			select
				id,
				starred_by_did,
				rkey,
				repo_at,
				created
			from stars;

			drop table stars;
			alter table stars_new rename to stars;

			create index if not exists idx_stars_created on stars(created);
			create index if not exists idx_stars_subject_at_created on stars(subject_at, created);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-avatar-to-profile", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table profile add column avatar text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "remove-profile-stats-column-constraint", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
		-- create new table without the check constraint
		create table profile_stats_new (
			id integer primary key autoincrement,
			did text not null,
			kind text not null, -- no constraint this time
			foreign key (did) references profile(did) on delete cascade
		);

		-- copy data from old table
		insert into profile_stats_new (id, did, kind)
		select id, did, kind
		from profile_stats;

		-- drop old table
		drop table profile_stats;

		-- rename new table
		alter table profile_stats_new rename to profile_stats;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-preferred-handle-profile", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table profile add column preferred_handle text;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-repo-did-column", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table repos add column repo_did text;
			create unique index if not exists idx_repos_repo_did on repos(repo_did);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-pds-rewrite-status", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists pds_rewrite_status (
				id          integer primary key autoincrement,
				user_did    text not null,
				repo_did    text not null,
				record_nsid text not null,
				record_rkey text not null,
				old_repo_at text not null,
				status      text not null default 'pending',
				updated_at  text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				unique(user_did, record_nsid, record_rkey)
			);
			create index if not exists idx_pds_rewrite_user on pds_rewrite_status(user_did, status);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-pipelines-repo-did", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table pipelines add column repo_did text;
			create index if not exists idx_pipelines_repo_did on pipelines(repo_did);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "migrate-knots-to-repo-dids", func(tx *sql.Tx) error {
		_, err := tx.Exec(`update registrations set needs_upgrade = 1`)
		return err
	})

	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "drop-pinned-repos-at-uri-fk", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists profile_pinned_repositories_new (
				id integer primary key autoincrement,
				did text not null,
				pin text not null,

				unique(did, pin),
				foreign key (did) references profile(did) on delete cascade
			);

			insert into profile_pinned_repositories_new (id, did, pin)
			select id, did, at_uri from profile_pinned_repositories;

			drop table profile_pinned_repositories;

			alter table profile_pinned_repositories_new rename to profile_pinned_repositories;
		`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	orm.RunMigration(conn, logger, "reset-profile-pin-rewrites", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			update pds_rewrite_status
			set status = 'pending',
			    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
			where record_nsid = 'sh.tangled.actor.profile'
			  and status = 'done'
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-blob-data-to-pull-submissions", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			alter table pull_submissions add column patch_blob_ref text;
			alter table pull_submissions add column patch_blob_mime text;
			alter table pull_submissions add column patch_blob_size integer;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "replace-parent-change-id-with-aturi", func(tx *sql.Tx) error {
		// add new column
		_, err := tx.Exec(`
			alter table pulls add column dependent_on text;
		`)
		if err != nil {
			return err
		}

		// populate dependent_on with at_uri of the parent
		_, err = tx.Exec(`
			update pulls
			set dependent_on = (
				select at_uri
				from pulls as parent
				where parent.stack_id = pulls.stack_id
				and parent.change_id = pulls.parent_change_id
			)
			where parent_change_id is not null;
		`)
		if err != nil {
			return err
		}

		// drop old columns
		_, err = tx.Exec(`
			alter table pulls drop column parent_change_id;
			alter table pulls drop column stack_id;
		`)

		return err
	})

	orm.RunMigration(conn, logger, "add-pds-migration", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists pds_migration (
				name text not null,

				-- record at_uri
				did        text not null,
				collection text not null,
				rkey       text not null,

				status      text not null default 'pending',
				error_msg   text,
				retry_count integer not null default 0,
				retry_after integer not null default 0,
				updated_at  text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

				unique(name, did, collection, rkey)
			);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "unify-pds-record-migration-table", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			insert into pds_migration (
				name,
				did,
				collection,
				rkey,
				status,
				updated_at
			)
			select
				'add-repo-did',
				user_did,
				record_nsid,
				record_rkey,
				status,
				updated_at
			from pds_rewrite_status;

			drop table pds_rewrite_status;
		`)
		return err
	})

	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "add-id-to-vouches", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table vouches_new (
				id integer primary key autoincrement,
				did text not null,
				subject_did text not null,
				cid text not null,
				kind text not null default 'vouch',
				reason text,
				created_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				unique(did, subject_did),
				check (did <> subject_did),
				check (kind in ('vouch', 'denounce'))
			);

			insert into vouches_new (did, subject_did, cid, kind, reason, created_at)
			select did, subject_did, cid, kind, reason, created_at
			from vouches;

			drop table vouches;
			alter table vouches_new rename to vouches;
		`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "drop-pipeline-statuses-pipeline-fk", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists pipeline_statuses_new (
				id integer primary key autoincrement,
				spindle text not null,
				rkey text not null,

				pipeline_knot text not null,
				pipeline_rkey text not null,

				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				workflow text not null,
				status text not null,
				error text,
				exit_code integer not null default 0,

				unique (spindle, rkey)
			);

			insert into pipeline_statuses_new
			select * from pipeline_statuses;

			drop table pipeline_statuses;
			alter table pipeline_statuses_new rename to pipeline_statuses;
		`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	orm.RunMigration(conn, logger, "add-repo-renames", func(tx *sql.Tx) error {
		res, err := tx.Exec(`
			update repos
			set name = name || '-renamed-' || id || '-' || lower(hex(randomblob(4)))
			where id in (
				select id from (
					select id, row_number() over (
						partition by did, knot, name
						order by created desc, id desc
					) as rn
					from repos
				) where rn > 1
			);
		`)
		if err != nil {
			return err
		}
		if n, _ := res.RowsAffected(); n > 0 {
			logger.Warn("suffixed legacy duplicate repo names before adding unique index", "rows", n)
		}

		var remaining int
		if err := tx.QueryRow(`
			select count(*) from (
				select 1 from repos group by did, knot, name having count(*) > 1
			)
		`).Scan(&remaining); err != nil {
			return fmt.Errorf("checking for residual duplicate (did, knot, name) groups: %w", err)
		}
		if remaining > 0 {
			return fmt.Errorf("add-repo-renames: %d duplicate (did, knot, name) groups remain after suffix pass; manual cleanup required before unique index can be created", remaining)
		}

		_, err = tx.Exec(`
			create table if not exists repo_renames (
				owner_did  text not null,
				old_rkey   text not null,
				repo_did   text not null,
				renamed_at text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				primary key (owner_did, old_rkey)
			);
			create unique index if not exists idx_repos_owner_knot_name
				on repos(did, knot, name);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "repos-canonical-rkey-uniqueness", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			drop index if exists idx_repos_owner_knot_name;
			create unique index if not exists idx_repos_did_rkey
				on repos(did, rkey);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "repo-did-references", func(tx *sql.Tx) error {
		tables := []struct{ table, oldCol, newCol string }{
			{"issues", "repo_at", "repo_did"},
			{"pulls", "repo_at", "repo_did"},
			{"pull_comments", "repo_at", "repo_did"},
			{"stars", "subject_at", "subject_did"},
			{"artifacts", "repo_at", "repo_did"},
			{"webhooks", "repo_at", "repo_did"},
			{"repo_sites", "repo_at", "repo_did"},
			{"site_deploys", "repo_at", "repo_did"},
			{"collaborators", "repo_at", "repo_did"},
			{"repo_issue_seqs", "repo_at", "repo_did"},
			{"repo_pull_seqs", "repo_at", "repo_did"},
			{"repo_languages", "repo_at", "repo_did"},
			{"repo_labels", "repo_at", "repo_did"},
		}

		stmts := ""
		for _, t := range tables {
			stmts += fmt.Sprintf(
				`ALTER TABLE %s ADD COLUMN %s TEXT;
				 UPDATE %s SET %s = (SELECT repos.repo_did FROM repos WHERE repos.at_uri = %s.%s);
				 CREATE INDEX IF NOT EXISTS idx_%s_%s ON %s(%s);
				`, t.table, t.newCol, t.table, t.newCol, t.table, t.oldCol, t.table, t.newCol, t.table, t.newCol)
		}

		stmts += `ALTER TABLE pulls ADD COLUMN source_repo_did TEXT;
			UPDATE pulls SET source_repo_did = (SELECT repos.repo_did FROM repos WHERE repos.at_uri = pulls.source_repo_at);

			UPDATE profile_pinned_repositories SET pin = (
				SELECT repos.repo_did FROM repos WHERE repos.at_uri = profile_pinned_repositories.pin
			) WHERE pin LIKE 'at://%'
			  AND EXISTS (SELECT 1 FROM repos WHERE repos.at_uri = profile_pinned_repositories.pin AND repos.repo_did IS NOT NULL AND repos.repo_did != '');
		`

		_, err := tx.Exec(stmts)
		return err
	})

	orm.RunMigration(conn, logger, "backfill-pds-rewrites-star-issue-pull-collab", func(tx *sql.Tx) error {
		type source struct {
			userDidCol string
			table      string
			nsid       string
			fkCol      string
		}
		sources := []source{
			{"did", "stars", "sh.tangled.feed.star", "subject_at"},
			{"did", "issues", "sh.tangled.repo.issue", "repo_at"},
			{"owner_did", "pulls", "sh.tangled.repo.pull", "repo_at"},
			{"did", "collaborators", "sh.tangled.repo.collaborator", "repo_at"},
		}

		for _, src := range sources {
			_, err := tx.Exec(fmt.Sprintf(`
				INSERT INTO pds_migration (name, did, collection, rkey, status)
				SELECT 'add-repo-did', t.%s, '%s', t.rkey, 'pending'
				FROM %s t
				JOIN repos r ON r.at_uri = t.%s
				WHERE r.repo_did IS NOT NULL AND r.repo_did != ''
				ON CONFLICT(name, did, collection, rkey) DO NOTHING
			`, src.userDidCol, src.nsid, src.table, src.fkCol))
			if err != nil {
				return fmt.Errorf("backfill pds rewrites for %s: %w", src.table, err)
			}
		}

		return nil
	})

	orm.RunMigration(conn, logger, "backfill-pds-rewrites-profiles", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			INSERT INTO pds_migration (name, did, collection, rkey, status)
			SELECT DISTINCT 'add-repo-did', pp.did, 'sh.tangled.actor.profile', 'self', 'pending'
			FROM profile_pinned_repositories pp
			JOIN repos r ON r.at_uri = pp.pin
			WHERE pp.pin LIKE 'at://%'
			  AND r.repo_did IS NOT NULL AND r.repo_did != ''
			ON CONFLICT(name, did, collection, rkey) DO NOTHING
		`)
		if err != nil {
			return fmt.Errorf("backfill pds rewrites for profiles: %w", err)
		}
		return nil
	})

	conn.ExecContext(ctx, "pragma foreign_keys = off;")
	orm.RunMigration(conn, logger, "drop-old-at-uri-columns", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			CREATE TABLE repos_new (
				id          INTEGER PRIMARY KEY AUTOINCREMENT,
				did         TEXT NOT NULL,
				name        TEXT NOT NULL,
				knot        TEXT NOT NULL,
				rkey        TEXT NOT NULL,
				at_uri      TEXT NOT NULL UNIQUE,
				created     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				description TEXT CHECK (length(description) <= 200),
				source      TEXT,
				spindle     TEXT,
				website     TEXT,
				topics      TEXT,
				repo_did    TEXT,
				UNIQUE(did, rkey)
			);
			INSERT INTO repos_new (id, did, name, knot, rkey, at_uri, created, description, source, spindle, website, topics, repo_did)
			SELECT id, did, name, knot, rkey, at_uri, created, description, source, spindle, website, topics, repo_did
			FROM repos;
			DROP TABLE repos;
			ALTER TABLE repos_new RENAME TO repos;
			CREATE UNIQUE INDEX idx_repos_repo_did ON repos(repo_did);
			CREATE UNIQUE INDEX idx_repos_did_rkey ON repos(did, rkey);

			CREATE TABLE issues_new (
				id        INTEGER PRIMARY KEY AUTOINCREMENT,
				did       TEXT NOT NULL,
				rkey      TEXT NOT NULL,
				at_uri    TEXT GENERATED ALWAYS AS ('at://' || did || '/' || 'sh.tangled.repo.issue' || '/' || rkey) STORED,
				repo_did  TEXT NOT NULL,
				issue_id  INTEGER NOT NULL,
				title     TEXT NOT NULL,
				body      TEXT NOT NULL,
				open      INTEGER NOT NULL DEFAULT 1,
				created   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				edited    TEXT,
				deleted   TEXT,
				UNIQUE(did, rkey),
				UNIQUE(repo_did, issue_id),
				UNIQUE(at_uri),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO issues_new (id, did, rkey, repo_did, issue_id, title, body, open, created, edited, deleted)
			SELECT id, did, rkey, repo_did, issue_id, title, body, open, created, edited, deleted
			FROM issues WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE issues;
			ALTER TABLE issues_new RENAME TO issues;
			CREATE INDEX idx_issues_repo_did ON issues(repo_did);

			CREATE TABLE pulls_new (
				id               INTEGER PRIMARY KEY AUTOINCREMENT,
				pull_id          INTEGER NOT NULL,
				at_uri           TEXT GENERATED ALWAYS AS ('at://' || owner_did || '/' || 'sh.tangled.repo.pull' || '/' || rkey) STORED,
				repo_did         TEXT NOT NULL,
				owner_did        TEXT NOT NULL,
				rkey             TEXT NOT NULL,
				title            TEXT NOT NULL,
				body             TEXT NOT NULL,
				target_branch    TEXT NOT NULL,
				state            INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2, 3)),
				source_branch    TEXT,
				source_repo_did  TEXT,
				change_id        TEXT,
				dependent_on     TEXT,
				created          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				UNIQUE(repo_did, pull_id),
				UNIQUE(at_uri),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO pulls_new (id, pull_id, repo_did, owner_did, rkey, title, body, target_branch, state, source_branch, source_repo_did, change_id, dependent_on, created)
			SELECT id, pull_id, repo_did, owner_did, rkey, title, body, target_branch, state, source_branch, source_repo_did, change_id, dependent_on, created
			FROM pulls WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE pulls;
			ALTER TABLE pulls_new RENAME TO pulls;
			CREATE INDEX idx_pulls_repo_did ON pulls(repo_did);
			CREATE INDEX idx_pulls_source_repo_did ON pulls(source_repo_did);

			CREATE TABLE pull_comments_new (
				id            INTEGER PRIMARY KEY AUTOINCREMENT,
				pull_id       INTEGER NOT NULL,
				submission_id INTEGER NOT NULL,
				repo_did      TEXT NOT NULL,
				owner_did     TEXT NOT NULL,
				comment_at    TEXT NOT NULL,
				body          TEXT NOT NULL,
				created       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				FOREIGN KEY (repo_did, pull_id) REFERENCES pulls(repo_did, pull_id) ON DELETE CASCADE,
				FOREIGN KEY (submission_id) REFERENCES pull_submissions(id) ON DELETE CASCADE
			);
			INSERT INTO pull_comments_new (id, pull_id, submission_id, repo_did, owner_did, comment_at, body, created)
			SELECT id, pull_id, submission_id, repo_did, owner_did, comment_at, body, created
			FROM pull_comments WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE pull_comments;
			ALTER TABLE pull_comments_new RENAME TO pull_comments;
			CREATE INDEX idx_pull_comments_repo_did ON pull_comments(repo_did);

			CREATE TABLE stars_new (
				id           INTEGER PRIMARY KEY AUTOINCREMENT,
				did          TEXT NOT NULL,
				rkey         TEXT NOT NULL,
				subject_type TEXT NOT NULL CHECK (subject_type IN ('repo', 'string')),
				subject      TEXT NOT NULL,
				created      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				UNIQUE(did, rkey),
				UNIQUE(did, subject)
			);
			INSERT INTO stars_new (id, did, rkey, subject_type, subject, created)
				SELECT id, did, rkey, 'repo', subject_did, created
				FROM stars
				WHERE subject_did IS NOT NULL AND subject_did != '';
			INSERT OR IGNORE INTO stars_new (id, did, rkey, subject_type, subject, created)
				SELECT id, did, rkey, 'string', subject_at, created
				FROM stars
				WHERE (subject_did IS NULL OR subject_did = '')
				  AND subject_at LIKE 'at://%/sh.tangled.string/%';
			DROP TABLE stars;
			ALTER TABLE stars_new RENAME TO stars;
			CREATE INDEX idx_stars_subject ON stars(subject);
			CREATE INDEX idx_stars_subject_type ON stars(subject_type);
			CREATE INDEX idx_stars_created ON stars(created);

			CREATE TABLE collaborators_new (
				id          INTEGER PRIMARY KEY AUTOINCREMENT,
				did         TEXT NOT NULL,
				rkey        TEXT,
				subject_did TEXT NOT NULL,
				repo_did    TEXT NOT NULL,
				created     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				UNIQUE(did, rkey),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO collaborators_new (id, did, rkey, subject_did, repo_did, created)
			SELECT id, did, NULLIF(rkey, ''), subject_did, repo_did, created
			FROM collaborators WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE collaborators;
			ALTER TABLE collaborators_new RENAME TO collaborators;
			CREATE INDEX idx_collaborators_repo_did ON collaborators(repo_did);

			CREATE TABLE artifacts_new (
				id       INTEGER PRIMARY KEY AUTOINCREMENT,
				did      TEXT NOT NULL,
				rkey     TEXT NOT NULL,
				repo_did TEXT NOT NULL,
				tag      BINARY(20) NOT NULL,
				created  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				blob_cid TEXT NOT NULL,
				name     TEXT NOT NULL,
				size     INTEGER NOT NULL DEFAULT 0,
				mimetype TEXT NOT NULL DEFAULT '*/*',
				UNIQUE(did, rkey),
				UNIQUE(repo_did, tag, name),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO artifacts_new (id, did, rkey, repo_did, tag, created, blob_cid, name, size, mimetype)
			SELECT id, did, rkey, repo_did, tag, created, blob_cid, name, size, mimetype
			FROM artifacts WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE artifacts;
			ALTER TABLE artifacts_new RENAME TO artifacts;
			CREATE INDEX idx_artifacts_repo_did ON artifacts(repo_did);

			CREATE TABLE webhooks_new (
				id         INTEGER PRIMARY KEY AUTOINCREMENT,
				repo_did   TEXT NOT NULL,
				url        TEXT NOT NULL,
				secret     TEXT,
				active     INTEGER NOT NULL DEFAULT 1,
				events     TEXT NOT NULL,
				created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO webhooks_new (id, repo_did, url, secret, active, events, created_at, updated_at)
			SELECT id, repo_did, url, secret, active, events, created_at, updated_at
			FROM webhooks WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE webhooks;
			ALTER TABLE webhooks_new RENAME TO webhooks;
			CREATE INDEX idx_webhooks_repo_did ON webhooks(repo_did);

			CREATE TABLE repo_sites_new (
				id       INTEGER PRIMARY KEY AUTOINCREMENT,
				repo_did TEXT NOT NULL UNIQUE,
				branch   TEXT NOT NULL,
				dir      TEXT NOT NULL DEFAULT '/',
				is_index INTEGER NOT NULL DEFAULT 0,
				created  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				updated  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO repo_sites_new (id, repo_did, branch, dir, is_index, created, updated)
			SELECT id, repo_did, branch, dir, is_index, created, updated
			FROM repo_sites WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE repo_sites;
			ALTER TABLE repo_sites_new RENAME TO repo_sites;

			CREATE TABLE site_deploys_new (
				id         INTEGER PRIMARY KEY AUTOINCREMENT,
				repo_did   TEXT NOT NULL,
				branch     TEXT NOT NULL,
				dir        TEXT NOT NULL DEFAULT '/',
				commit_sha TEXT NOT NULL DEFAULT '',
				status     TEXT NOT NULL CHECK (status IN ('success', 'failure')),
				trigger    TEXT NOT NULL CHECK (trigger IN ('config_change', 'push')),
				error      TEXT NOT NULL DEFAULT '',
				created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO site_deploys_new (id, repo_did, branch, dir, commit_sha, status, trigger, error, created_at)
			SELECT id, repo_did, branch, dir, commit_sha, status, trigger, error, created_at
			FROM site_deploys WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE site_deploys;
			ALTER TABLE site_deploys_new RENAME TO site_deploys;
			CREATE INDEX idx_site_deploys_repo_did ON site_deploys(repo_did);

			CREATE TABLE repo_issue_seqs_new (
				repo_did      TEXT PRIMARY KEY,
				next_issue_id INTEGER NOT NULL DEFAULT 1,
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO repo_issue_seqs_new (repo_did, next_issue_id)
			SELECT repo_did, next_issue_id
			FROM repo_issue_seqs WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE repo_issue_seqs;
			ALTER TABLE repo_issue_seqs_new RENAME TO repo_issue_seqs;

			CREATE TABLE repo_pull_seqs_new (
				repo_did     TEXT PRIMARY KEY,
				next_pull_id INTEGER NOT NULL DEFAULT 1,
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO repo_pull_seqs_new (repo_did, next_pull_id)
			SELECT repo_did, next_pull_id
			FROM repo_pull_seqs WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE repo_pull_seqs;
			ALTER TABLE repo_pull_seqs_new RENAME TO repo_pull_seqs;

			CREATE TABLE repo_languages_new (
				id             INTEGER PRIMARY KEY AUTOINCREMENT,
				repo_did       TEXT NOT NULL,
				ref            TEXT NOT NULL,
				is_default_ref INTEGER NOT NULL DEFAULT 0,
				language       TEXT NOT NULL,
				bytes          INTEGER NOT NULL CHECK (bytes >= 0),
				UNIQUE(repo_did, ref, language),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO repo_languages_new (id, repo_did, ref, is_default_ref, language, bytes)
			SELECT id, repo_did, ref, is_default_ref, language, bytes
			FROM repo_languages WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE repo_languages;
			ALTER TABLE repo_languages_new RENAME TO repo_languages;

			CREATE TABLE repo_labels_new (
				id       INTEGER PRIMARY KEY AUTOINCREMENT,
				repo_did TEXT NOT NULL,
				label_at TEXT NOT NULL,
				UNIQUE(repo_did, label_at),
				FOREIGN KEY (repo_did) REFERENCES repos(repo_did) ON DELETE CASCADE
			);
			INSERT INTO repo_labels_new (id, repo_did, label_at)
			SELECT id, repo_did, label_at
			FROM repo_labels WHERE repo_did IS NOT NULL AND repo_did != '';
			DROP TABLE repo_labels;
			ALTER TABLE repo_labels_new RENAME TO repo_labels;
		`)
		return err
	})
	conn.ExecContext(ctx, "pragma foreign_keys = on;")

	orm.RunMigration(conn, logger, "migrate-knots-to-repo-did-rename", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			update registrations set needs_upgrade = 1;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "drop-ghost-comments-table", func(tx *sql.Tx) error {
		_, err := tx.Exec(`DROP TABLE IF EXISTS comments`)
		return err
	})

	orm.RunMigration(conn, logger, "add-knot-members-table", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			create table if not exists knot_members (
				id integer primary key autoincrement,
				did text not null,
				rkey text not null,
				domain text not null,
				subject text not null,
				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
				unique (did, domain, subject)
			);
			create index if not exists idx_knot_members_did_rkey on knot_members(did, rkey);
		`)
		return err
	})

	orm.RunMigration(conn, logger, "add-comments-table", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			drop table if exists comments;

			create table comments (
				-- identifiers
				id integer primary key autoincrement,

				did        text not null,
				collection text not null default 'sh.tangled.feed.comment',
				rkey       text not null,
				at_uri     text generated always as ('at://' || did || '/' || collection || '/' || rkey) stored,
				cid        text,

				-- content
				subject_uri text not null, -- at_uri of subject (issue, pr, string)
				subject_cid text not null, -- cid of subject

				body_text     text not null,
				body_original text,
				body_blobs    text, -- json

				created text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

				reply_to_uri text, -- at_uri of parent comment
				reply_to_cid text, -- cid of parent comment

				pull_round_idx integer, -- pull round index. required when subject is sh.tangled.repo.pull

				-- appview-local information
				edited text,
				deleted text,

				unique(did, collection, rkey)
			);

			insert into comments (
				did,
				collection,
				rkey,
				subject_uri,
				subject_cid, -- we need to know cid
				body_text,
				created,
				reply_to_uri,
				reply_to_cid, -- we need to know cid
				edited,
				deleted
			)
			select
				did,
				'sh.tangled.repo.issue.comment',
				rkey,
				issue_at,
				'',
				body,
				created,
				reply_to,
				'',
				edited,
				deleted
			from issue_comments
			where rkey is not null;

			insert into comments (
				did,
				collection,
				rkey,
				subject_uri,
				subject_cid, -- we need to know cid
				body_text,
				created,
				pull_round_idx
			)
			select
				c.owner_did,
				'sh.tangled.repo.pull.comment',
				substr(
					substr(c.comment_at, 6 + instr(substr(c.comment_at, 6), '/')), -- nsid/rkey
					instr(
						substr(c.comment_at, 6 + instr(substr(c.comment_at, 6), '/')), -- nsid/rkey
						'/'
					) + 1
				), -- rkey
				p.at_uri,
				'',
				c.body,
				c.created,
				s.round_number
			from pull_comments c
			join pulls p on c.repo_did = p.repo_did and c.pull_id = p.pull_id
			join pull_submissions s on s.id = c.submission_id;
		`)
		return err
	})

	orm.RunMigration(conn, logger, "migrate-legacy-comments", func(tx *sql.Tx) error {
		_, err := tx.Exec(`
			insert into pds_migration (name, did, collection, rkey)
			select
				'use-feed-comment',
				did,
				collection,
				rkey
			from comments
			where collection <> 'sh.tangled.feed.comment';
		`)
		return err
	})

	return &DB{
		db,
		logger,
	}, nil
}

func (d *DB) Close() error {
	return d.DB.Close()
}
