package spindle

import (
	"context"
	"database/sql"
	"fmt"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/rbac"
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

func newTestSpindleDB(t *testing.T) (*db.DB, *rbac.Enforcer) {
	t.Helper()
	p := filepath.Join(t.TempDir(), "spindle.db")
	d, err := db.Make(context.Background(), p)
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	e, err := rbac.NewEnforcer(p)
	if err != nil {
		t.Fatalf("rbac.NewEnforcer: %v", err)
	}
	e.E.EnableAutoSave(true)
	return d, e
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

func mustAddCollab(t *testing.T, d *db.DB, owner, rkey, subject, repoDid string) {
	t.Helper()
	if err := d.AddRepoCollaborator(db.RepoCollaborator{
		OwnerDid: syntax.DID(owner),
		Rkey:     syntax.RecordKey(rkey),
		Subject:  syntax.DID(subject),
		RepoDid:  syntax.DID(repoDid),
	}); err != nil {
		t.Fatalf("AddRepoCollaborator(%s): %v", rkey, err)
	}
}

func TestMigrateLegacyRepoSecrets_NameCandidate(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, _ := newTestSpindleDB(t)
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
	d, _ := newTestSpindleDB(t)
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
	d, _ := newTestSpindleDB(t)
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
	d, _ := newTestSpindleDB(t)
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
	d, _ := newTestSpindleDB(t)
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
	d, _ := newTestSpindleDB(t)
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

func TestMigrateLegacyRepoCasbin_NameCandidate(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	displayName := "myrepo"
	rkey := "3kspindlerkey00a"
	collab := "did:plc:limpet"
	oldNameKey := owner + "/" + displayName
	oldRkeyKey := owner + "/" + rkey

	mustAddCollab(t, d, owner, "3kcollabrkey0001", collab, repoDid)

	if err := e.AddRepo(owner, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed AddRepo at Name key: %v", err)
	}
	if err := e.AddCollaborator(collab, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed AddCollaborator at Name key: %v", err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	if got, err := e.IsSettingsAllowed(owner, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("owner should have settings at new repoDid key, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("collab should have settings at new repoDid key, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("owner Name-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("collab Name-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldRkeyKey); err != nil || got {
		t.Errorf("owner rkey-keyed policy should be absent (never added), allowed=%v err=%v", got, err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	if got, err := e.IsSettingsAllowed(collab, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("collab settings still expected after idempotent re-run, allowed=%v err=%v", got, err)
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"legacy-casbin-rekey:"+repoDid+":"+rkey,
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 1 {
		t.Errorf("expected per-repo flag recorded exactly once, got %d", marked)
	}
}

func TestMigrateLegacyRepoCasbin_RkeyCandidate(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	displayName := "myrepo"
	rkey := "3kspindlerkey00a"
	collab := "did:plc:limpet"
	oldRkeyKey := owner + "/" + rkey

	mustAddCollab(t, d, owner, "3kcollabrkey0001", collab, repoDid)

	if err := e.AddRepo(owner, rbacDomain, oldRkeyKey); err != nil {
		t.Fatalf("seed AddRepo at rkey: %v", err)
	}
	if err := e.AddCollaborator(collab, rbacDomain, oldRkeyKey); err != nil {
		t.Fatalf("seed AddCollaborator at rkey: %v", err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	if got, err := e.IsSettingsAllowed(owner, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("owner should have settings at new repoDid key, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("collab should have settings at new repoDid key, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldRkeyKey); err != nil || got {
		t.Errorf("owner rkey-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, oldRkeyKey); err != nil || got {
		t.Errorf("collab rkey-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
}

func TestMigrateLegacyRepoCasbin_BothCandidates(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	displayName := "myrepo"
	rkey := "3kspindlerkey00a"
	collab := "did:plc:limpet"
	oldNameKey := owner + "/" + displayName
	oldRkeyKey := owner + "/" + rkey

	mustAddCollab(t, d, owner, "3kcollabrkey0001", collab, repoDid)

	if err := e.AddRepo(owner, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed AddRepo at Name key: %v", err)
	}
	if err := e.AddRepo(owner, rbacDomain, oldRkeyKey); err != nil {
		t.Fatalf("seed AddRepo at rkey: %v", err)
	}
	if err := e.AddCollaborator(collab, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed AddCollaborator at Name key: %v", err)
	}
	if err := e.AddCollaborator(collab, rbacDomain, oldRkeyKey); err != nil {
		t.Fatalf("seed AddCollaborator at rkey: %v", err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("owner Name-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldRkeyKey); err != nil || got {
		t.Errorf("owner rkey-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("collab Name-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(collab, rbacDomain, oldRkeyKey); err != nil || got {
		t.Errorf("collab rkey-keyed policy should be removed, allowed=%v err=%v", got, err)
	}
}

func TestMigrateLegacyRepoCasbin_BothEmpty(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:boltless")

	migrateLegacyRepoCasbin(ctx, d, e, logger, owner, "", "", repoDid)

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name like ?`,
		"legacy-casbin-rekey:"+repoDid.String()+":%",
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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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
	d, _ := newTestSpindleDB(t)

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

func TestMigrateLegacyRepoCasbin_MultipleCollabsAllRekeyed(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	displayName := "myrepo"
	rkey := "3kspindlerkey00a"
	oldNameKey := owner + "/" + displayName
	collabs := []string{"did:plc:limpet", "did:plc:nautilus", "did:plc:whelk", "did:plc:cuttle"}

	var addCollabRows func(rest []string, idx int)
	addCollabRows = func(rest []string, idx int) {
		if len(rest) == 0 {
			return
		}
		mustAddCollab(t, d, owner, fmt.Sprintf("3kcollabrkey%04d", idx), rest[0], repoDid)
		addCollabRows(rest[1:], idx+1)
	}
	addCollabRows(collabs, 0)

	if err := e.AddRepo(owner, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed owner: %v", err)
	}
	var seedAll func(rest []string) error
	seedAll = func(rest []string) error {
		if len(rest) == 0 {
			return nil
		}
		if err := e.AddCollaborator(rest[0], rbacDomain, oldNameKey); err != nil {
			return err
		}
		return seedAll(rest[1:])
	}
	if err := seedAll(collabs); err != nil {
		t.Fatalf("seed collab policies: %v", err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	var assertEach func(rest []string)
	assertEach = func(rest []string) {
		if len(rest) == 0 {
			return
		}
		c := rest[0]
		if got, err := e.IsSettingsAllowed(c, rbacDomain, repoDid); err != nil || !got {
			t.Errorf("collab %s should have settings at repoDid, allowed=%v err=%v", c, got, err)
		}
		if got, err := e.IsPushAllowed(c, rbacDomain, repoDid); err != nil || !got {
			t.Errorf("collab %s should have push at repoDid, allowed=%v err=%v", c, got, err)
		}
		if got, err := e.IsSettingsAllowed(c, rbacDomain, oldNameKey); err != nil || got {
			t.Errorf("collab %s old policy should be wiped, allowed=%v err=%v", c, got, err)
		}
		assertEach(rest[1:])
	}
	assertEach(collabs)
}

func TestMigrateLegacyRepoCasbin_RenameSiblingsEachWiped(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := syntax.DID("did:plc:akshay")
	repoDid := syntax.DID("did:plc:di4gol2smljyj6gjnjdu5qrg")
	siblings := []string{"pre-rename-life", "i-renamed-this", "post-rename-rename", "post-rename-renamed-again"}

	var seedAll func(rest []string) error
	seedAll = func(rest []string) error {
		if len(rest) == 0 {
			return nil
		}
		if err := e.AddRepo(owner.String(), rbacDomain, owner.String()+"/"+rest[0]); err != nil {
			return err
		}
		return seedAll(rest[1:])
	}
	if err := seedAll(siblings); err != nil {
		t.Fatalf("seed siblings: %v", err)
	}

	var run func(rest []string)
	run = func(rest []string) {
		if len(rest) == 0 {
			return
		}
		migrateLegacyRepoCasbin(ctx, d, e, logger, owner, "", syntax.RecordKey(rest[0]), repoDid)
		run(rest[1:])
	}
	run(siblings)

	var assertWiped func(rest []string)
	assertWiped = func(rest []string) {
		if len(rest) == 0 {
			return
		}
		key := owner.String() + "/" + rest[0]
		if got, err := e.IsSettingsAllowed(owner.String(), rbacDomain, key); err != nil || got {
			t.Errorf("rename sibling %s should be wiped, allowed=%v err=%v", rest[0], got, err)
		}
		assertWiped(rest[1:])
	}
	assertWiped(siblings)

	if got, err := e.IsSettingsAllowed(owner.String(), rbacDomain, repoDid.String()); err != nil || !got {
		t.Errorf("owner should retain settings at repoDid, allowed=%v err=%v", got, err)
	}
}

func TestMigrateLegacyRepoCasbin_StrandedCollabWiped(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d, e := newTestSpindleDB(t)

	if err := e.AddSpindle(rbacDomain); err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	displayName := "myrepo"
	rkey := "3kspindlerkey00a"
	strandedCollab := "did:plc:nautilus"
	oldNameKey := owner + "/" + displayName

	if err := e.AddRepo(owner, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed AddRepo at Name key: %v", err)
	}
	if err := e.AddCollaborator(strandedCollab, rbacDomain, oldNameKey); err != nil {
		t.Fatalf("seed stranded collab at Name key: %v", err)
	}

	migrateLegacyRepoCasbin(ctx, d, e, logger, syntax.DID(owner), displayName, syntax.RecordKey(rkey), syntax.DID(repoDid))

	if got, err := e.IsSettingsAllowed(strandedCollab, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("stranded collab should be wiped from old key, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, oldNameKey); err != nil || got {
		t.Errorf("owner old policy should be wiped, allowed=%v err=%v", got, err)
	}
	if got, err := e.IsSettingsAllowed(owner, rbacDomain, repoDid); err != nil || !got {
		t.Errorf("owner should have settings at new repoDid key, allowed=%v err=%v", got, err)
	}
}
