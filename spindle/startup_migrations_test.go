package spindle

import (
	"context"
	"database/sql"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/secrets"
)

func seedTapDB(t *testing.T, path string) {
	t.Helper()
	tdb, err := sql.Open("sqlite3", path)
	if err != nil {
		t.Fatalf("open tap db: %v", err)
	}
	defer tdb.Close()
	if _, err := tdb.Exec(`
		create table repos (
			did         text primary key,
			state       text not null default 'pending',
			status      text not null default 'active',
			handle      text default '',
			rev         text default '',
			prev_data   text default '',
			error_msg   text default '',
			retry_count integer not null default 0,
			retry_after integer not null default 0
		);
		create table repo_records (
			did        text not null,
			collection text not null,
			rkey       text not null,
			cid        text not null,
			primary key (did, collection, rkey)
		);
	`); err != nil {
		t.Fatalf("create tap tables: %v", err)
	}
}

func tapRepoState(t *testing.T, path, did string) string {
	t.Helper()
	tdb, err := sql.Open("sqlite3", path)
	if err != nil {
		t.Fatalf("open tap db: %v", err)
	}
	defer tdb.Close()
	var state string
	if err := tdb.QueryRow(`select state from repos where did = ?`, did).Scan(&state); err != nil {
		t.Fatalf("query state for %s: %v", did, err)
	}
	return state
}

func tapRecordCount(t *testing.T, path string) int {
	t.Helper()
	tdb, err := sql.Open("sqlite3", path)
	if err != nil {
		t.Fatalf("open tap db: %v", err)
	}
	defer tdb.Close()
	var n int
	if err := tdb.QueryRow(`select count(*) from repo_records`).Scan(&n); err != nil {
		t.Fatalf("count repo_records: %v", err)
	}
	return n
}

func newTestSpindleDB(t *testing.T) *db.DB {
	t.Helper()
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func newTestVault(t *testing.T) *secrets.SqliteManager {
	t.Helper()
	vault, err := secrets.NewSQLiteManager(filepath.Join(t.TempDir(), "vault.db"))
	if err != nil {
		t.Fatalf("vault.New: %v", err)
	}
	return vault
}

func mustAddSecret(t *testing.T, vault secrets.Manager, repo, key, value string, createdAt time.Time, by string) {
	t.Helper()
	err := vault.AddSecret(context.Background(), secrets.UnlockedSecret{
		Repo:      secrets.RepoIdentifier(repo),
		Key:       key,
		Value:     value,
		CreatedAt: createdAt,
		CreatedBy: syntax.DID(by),
	})
	if err != nil {
		t.Fatalf("AddSecret(%s/%s): %v", repo, key, err)
	}
}

func TestMigrateLegacyRepoSecrets_NameCandidate(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")
	displayName := "myrepo"
	rkey := syntax.RecordKey("3kspindlerkey00a")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldNameKey := owner.String() + "/" + displayName

	mustAddSecret(t, vault, oldNameKey, "API_KEY", "alpha", created, owner.String())
	mustAddSecret(t, vault, oldNameKey, "DB_PASSWORD", "bravo", created.Add(1*time.Hour), owner.String())

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, displayName, rkey, repoDid)

	copied, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(new): %v", err)
	}
	if len(copied) != 2 {
		t.Fatalf("expected 2 secrets under repo_did key, got %d", len(copied))
	}

	want := map[string]struct {
		value     string
		createdAt time.Time
	}{
		"API_KEY":     {"alpha", created},
		"DB_PASSWORD": {"bravo", created.Add(1 * time.Hour)},
	}
	for _, s := range copied {
		w, ok := want[s.Key]
		if !ok {
			t.Errorf("unexpected key %q under %s", s.Key, repoDid)
			continue
		}
		if s.Value != w.value {
			t.Errorf("%s: value got %q, want %q", s.Key, s.Value, w.value)
		}
		if !s.CreatedAt.Equal(w.createdAt) {
			t.Errorf("%s: CreatedAt got %s, want %s", s.Key, s.CreatedAt, w.createdAt)
		}
		if string(s.Repo) != repoDid.String() {
			t.Errorf("%s: Repo got %s, want %s", s.Key, s.Repo, repoDid)
		}
	}

	orig, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(oldNameKey))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(old): %v", err)
	}
	if len(orig) != 2 {
		t.Errorf("expected old-key secrets preserved, got %d", len(orig))
	}

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, displayName, rkey, repoDid)
	again, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(new) after re-run: %v", err)
	}
	if len(again) != 2 {
		t.Errorf("re-run should not duplicate or drop secrets, got %d", len(again))
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"legacy-secret-copy:"+repoDid.String()+":"+rkey.String(),
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 1 {
		t.Errorf("expected per-repo flag recorded exactly once, got %d", marked)
	}
}

func TestMigrateLegacyRepoSecrets_RkeyCandidate(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")
	displayName := "myrepo"
	rkey := syntax.RecordKey("3kspindlerkey00a")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldRkeyKey := owner.String() + "/" + rkey.String()

	mustAddSecret(t, vault, oldRkeyKey, "API_KEY", "alpha", created, owner.String())
	mustAddSecret(t, vault, oldRkeyKey, "DB_PASSWORD", "bravo", created, owner.String())

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, displayName, rkey, repoDid)

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 2 {
		t.Fatalf("expected 2 secrets copied via rkey candidate, got %d", len(got))
	}
}

func TestMigrateLegacyRepoSecrets_BothCandidates(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")
	displayName := "myrepo"
	rkey := syntax.RecordKey("3kspindlerkey00a")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldNameKey := owner.String() + "/" + displayName
	oldRkeyKey := owner.String() + "/" + rkey.String()

	mustAddSecret(t, vault, oldNameKey, "FROM_NAME", "n", created, owner.String())
	mustAddSecret(t, vault, oldRkeyKey, "FROM_RKEY", "r", created, owner.String())

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, displayName, rkey, repoDid)

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 2 {
		t.Fatalf("expected 2 secrets merged from both candidates, got %d", len(got))
	}
	seen := map[string]string{}
	for _, s := range got {
		seen[s.Key] = s.Value
	}
	if seen["FROM_NAME"] != "n" {
		t.Errorf("FROM_NAME missing or wrong value: %q", seen["FROM_NAME"])
	}
	if seen["FROM_RKEY"] != "r" {
		t.Errorf("FROM_RKEY missing or wrong value: %q", seen["FROM_RKEY"])
	}
}

func TestMigrateLegacyRepoSecrets_PreExistingTakesPriority(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")
	displayName := "myrepo"
	rkey := syntax.RecordKey("3kspindlerkey00a")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldKey := owner.String() + "/" + displayName

	mustAddSecret(t, vault, oldKey, "API_KEY", "alpha", created, owner.String())
	mustAddSecret(t, vault, oldKey, "DB_PASSWORD", "bravo", created, owner.String())
	mustAddSecret(t, vault, repoDid.String(), "API_KEY", "pre-existing", created.Add(-24*time.Hour), owner.String())

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, displayName, rkey, repoDid)

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 2 {
		t.Fatalf("expected 2 secrets under repo_did key, got %d", len(got))
	}
	for _, s := range got {
		if s.Key == "API_KEY" && s.Value != "pre-existing" {
			t.Errorf("API_KEY should preserve pre-existing value, got %q", s.Value)
		}
		if s.Key == "DB_PASSWORD" && s.Value != "bravo" {
			t.Errorf("DB_PASSWORD should be copied, got %q", s.Value)
		}
	}
}

func TestMigrateLegacyRepoSecrets_EmptyName(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")
	rkey := syntax.RecordKey("3kspindlerkey00a")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldRkeyKey := owner.String() + "/" + rkey.String()
	mustAddSecret(t, vault, oldRkeyKey, "API_KEY", "alpha", created, owner.String())

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, "", rkey, repoDid)

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 1 {
		t.Errorf("expected 1 secret via rkey candidate when name empty, got %d", len(got))
	}
}

func TestMigrateLegacyRepoSecrets_BothEmpty(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	migrateLegacyRepoSecrets(ctx, d, vault, logger, owner, "", "", repoDid)

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 0 {
		t.Errorf("expected no work when both name and rkey empty, got %d secrets", len(got))
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name like ?`,
		"legacy-secret-copy:"+repoDid.String()+":%",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 0 {
		t.Errorf("empty inputs should not record flag, got %d", marked)
	}
}

func TestNudgeTapForResync(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	tapPath := filepath.Join(t.TempDir(), "tap.db")
	seedTapDB(t, tapPath)

	tdb, err := sql.Open("sqlite3", tapPath)
	if err != nil {
		t.Fatalf("open tap db: %v", err)
	}
	if _, err := tdb.Exec(`insert into repos (did, state) values
		('did:plc:akshay', 'active'),
		('did:plc:boltless', 'error'),
		('did:plc:limpet', 'pending')
	`); err != nil {
		t.Fatalf("seed repos: %v", err)
	}
	if _, err := tdb.Exec(`insert into repo_records (did, collection, rkey, cid) values
		('did:plc:akshay', 'sh.tangled.repo', '3kspindlerkey00a', 'bafyone'),
		('did:plc:boltless', 'sh.tangled.repo', '3kspindlerkey00b', 'bafytwo')
	`); err != nil {
		t.Fatalf("seed records: %v", err)
	}
	tdb.Close()

	if err := nudgeTapForResync(ctx, d, tapPath, logger); err != nil {
		t.Fatalf("nudgeTapForResync: %v", err)
	}

	if got := tapRecordCount(t, tapPath); got != 0 {
		t.Errorf("expected repo_records cleared, got %d", got)
	}
	if got := tapRepoState(t, tapPath, "did:plc:akshay"); got != "desynchronized" {
		t.Errorf("active should flip to desynchronized, got %s", got)
	}
	if got := tapRepoState(t, tapPath, "did:plc:boltless"); got != "desynchronized" {
		t.Errorf("error should flip to desynchronized, got %s", got)
	}
	if got := tapRepoState(t, tapPath, "did:plc:limpet"); got != "pending" {
		t.Errorf("pending should not be touched, got %s", got)
	}

	tdb2, err := sql.Open("sqlite3", tapPath)
	if err != nil {
		t.Fatalf("reopen tap db: %v", err)
	}
	if _, err := tdb2.Exec(`update repos set state = 'active' where did = 'did:plc:akshay'`); err != nil {
		t.Fatalf("reseed: %v", err)
	}
	tdb2.Close()

	if err := nudgeTapForResync(ctx, d, tapPath, logger); err != nil {
		t.Fatalf("nudgeTapForResync second run: %v", err)
	}
	if got := tapRepoState(t, tapPath, "did:plc:akshay"); got != "active" {
		t.Errorf("idempotent re-run should not touch state, got %s", got)
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"force-tap-repo-resync-v1",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 1 {
		t.Errorf("expected flag recorded exactly once, got %d", marked)
	}
}

func TestNudgeTapForResync_MissingDB(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	missing := filepath.Join(t.TempDir(), "absent.db")

	if err := nudgeTapForResync(ctx, d, missing, logger); err != nil {
		t.Fatalf("missing tap db should succeed: %v", err)
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"force-tap-repo-resync-v1",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 1 {
		t.Errorf("expected flag recorded even when tap db absent, got %d", marked)
	}
}

func TestNudgeTapForResync_EmptyPath(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	if err := nudgeTapForResync(ctx, d, "", logger); err == nil {
		t.Errorf("expected error for empty tap db path")
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"force-tap-repo-resync-v1",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 0 {
		t.Errorf("empty path should not mark flag, got %d", marked)
	}
}

func TestRunStartupMigrations_NonEmbedSkipsTapNudge(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	if err := runStartupMigrations(ctx, d, false, "", logger); err != nil {
		t.Fatalf("non-embed should not error on empty path: %v", err)
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"force-tap-repo-resync-v1",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 0 {
		t.Errorf("non-embed mode should skip tap nudge flag, got %d", marked)
	}
}

func TestCleanupOrphanRepos_DeletesWhenSiblingExists(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	owner := "did:plc:akshay"
	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'legacy_name', null, null),
		('k', ?, '3kspindlerkey00a', 'did:plc:boltless', '2024-01-01T00:00:00Z')`,
		owner, owner); err != nil {
		t.Fatalf("seed: %v", err)
	}

	if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
		t.Fatalf("cleanupOrphanRepos: %v", err)
	}

	var nullCount int
	if err := d.QueryRow(`select count(*) from repos where repo_did is null`).Scan(&nullCount); err != nil {
		t.Fatalf("null count: %v", err)
	}
	if nullCount != 0 {
		t.Errorf("orphan should be deleted when sibling exists, got %d remaining", nullCount)
	}

	var sibCount int
	if err := d.QueryRow(`select count(*) from repos where repo_did is not null`).Scan(&sibCount); err != nil {
		t.Fatalf("sibling count: %v", err)
	}
	if sibCount != 1 {
		t.Errorf("sibling row should be preserved, got %d", sibCount)
	}
}

func TestCleanupOrphanRepos_KeepsWhenAlone(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	owner := "did:plc:akshay"
	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'legacy_name', null, null)`, owner); err != nil {
		t.Fatalf("seed: %v", err)
	}

	if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
		t.Fatalf("cleanupOrphanRepos: %v", err)
	}

	var remaining int
	if err := d.QueryRow(`select count(*) from repos where owner = ?`, owner).Scan(&remaining); err != nil {
		t.Fatalf("count: %v", err)
	}
	if remaining != 1 {
		t.Errorf("orphan with no sibling should be kept (preserves owner registration), got %d", remaining)
	}
}

func TestCleanupOrphanRepos_PerOwnerScope(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	ownerA := "did:plc:akshay"
	ownerB := "did:plc:limpet"
	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'legacy_a',  null, null),
		('k', ?, '3krealkey', 'did:plc:boltless', '2024-01-01T00:00:00Z'),
		('k', ?, 'legacy_b',  null, null)`,
		ownerA, ownerA, ownerB); err != nil {
		t.Fatalf("seed: %v", err)
	}

	if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
		t.Fatalf("cleanupOrphanRepos: %v", err)
	}

	var ownerARows, ownerBRows int
	if err := d.QueryRow(`select count(*) from repos where owner = ?`, ownerA).Scan(&ownerARows); err != nil {
		t.Fatalf("count A: %v", err)
	}
	if ownerARows != 1 {
		t.Errorf("ownerA: orphan should be deleted (sibling exists), expected 1 row, got %d", ownerARows)
	}
	if err := d.QueryRow(`select count(*) from repos where owner = ?`, ownerB).Scan(&ownerBRows); err != nil {
		t.Fatalf("count B: %v", err)
	}
	if ownerBRows != 1 {
		t.Errorf("ownerB: orphan should be kept (no sibling), expected 1 row, got %d", ownerBRows)
	}
}

func TestCleanupOrphanRepos_EmptyStringRepoDid(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)

	owner := "did:plc:akshay"
	if _, err := d.Exec(`insert into repos (knot, owner, rkey, repo_did, created_at) values
		('k', ?, 'legacy_empty', '',                  null),
		('k', ?, '3krealkey',    'did:plc:boltless', '2024-01-01T00:00:00Z')`,
		owner, owner); err != nil {
		t.Fatalf("seed: %v", err)
	}

	if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
		t.Fatalf("cleanupOrphanRepos: %v", err)
	}

	var emptyCount int
	if err := d.QueryRow(`select count(*) from repos where coalesce(repo_did, '') = ''`).Scan(&emptyCount); err != nil {
		t.Fatalf("empty count: %v", err)
	}
	if emptyCount != 0 {
		t.Errorf("empty-string repo_did orphan should be deleted when sibling exists, got %d remaining", emptyCount)
	}
}
