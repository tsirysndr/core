package spindle

import (
	"context"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/secrets"
)

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

func mustAddRepo(t *testing.T, d *db.DB, knot, owner, rkey, repoDid string) {
	t.Helper()
	if err := d.AddRepo(db.Repo{
		Knot:    knot,
		Owner:   syntax.DID(owner),
		Rkey:    syntax.RecordKey(rkey),
		RepoDid: syntax.DID(repoDid),
	}); err != nil {
		t.Fatalf("AddRepo(%s): %v", rkey, err)
	}
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

func TestStartupMigrations_CopyOwnerRkeySecretsToRepoDid(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))

	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := "did:plc:akshay"
	migratedRepoDid := "did:plc:boltless"
	skippedRkey := "3kspindlerkey00b"
	migratedRkey := "3kspindlerkey00a"

	mustAddRepo(t, d, "knot.test", owner, migratedRkey, migratedRepoDid)
	mustAddRepo(t, d, "knot.test", owner, skippedRkey, "")

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldRepoKey := owner + "/" + migratedRkey
	skippedKey := owner + "/" + skippedRkey

	mustAddSecret(t, vault, oldRepoKey, "API_KEY", "alpha", created, owner)
	mustAddSecret(t, vault, oldRepoKey, "DB_PASSWORD", "bravo", created.Add(1*time.Hour), owner)
	mustAddSecret(t, vault, skippedKey, "STRAY", "delta", created, owner)

	if err := runStartupMigrations(ctx, d, vault, logger); err != nil {
		t.Fatalf("first migration run: %v", err)
	}

	copied, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(migratedRepoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(new): %v", err)
	}
	if len(copied) != 2 {
		t.Fatalf("expected 2 secrets under new repo_did key, got %d", len(copied))
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
			t.Errorf("unexpected key %q under %s", s.Key, migratedRepoDid)
			continue
		}
		if s.Value != w.value {
			t.Errorf("%s: value got %q, want %q", s.Key, s.Value, w.value)
		}
		if !s.CreatedAt.Equal(w.createdAt) {
			t.Errorf("%s: CreatedAt got %s, want %s", s.Key, s.CreatedAt, w.createdAt)
		}
		if string(s.Repo) != migratedRepoDid {
			t.Errorf("%s: Repo got %s, want %s", s.Key, s.Repo, migratedRepoDid)
		}
	}

	orig, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(oldRepoKey))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(old): %v", err)
	}
	if len(orig) != 2 {
		t.Errorf("expected old-key secrets preserved, got %d", len(orig))
	}

	stray, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(skippedKey))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(skipped): %v", err)
	}
	if len(stray) != 1 {
		t.Errorf("expected skipped repo's old-key secret untouched, got %d", len(stray))
	}

	if err := runStartupMigrations(ctx, d, vault, logger); err != nil {
		t.Fatalf("second migration run: %v", err)
	}

	again, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(migratedRepoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked(new) after re-run: %v", err)
	}
	if len(again) != 2 {
		t.Errorf("re-run should not duplicate or drop secrets, got %d", len(again))
	}

	var marked int
	if err := d.QueryRow(
		`select count(*) from migrations where name = ?`,
		"copy-owner-rkey-secrets-to-repo-did",
	).Scan(&marked); err != nil {
		t.Fatalf("query migrations: %v", err)
	}
	if marked != 1 {
		t.Errorf("expected migration recorded exactly once, got %d", marked)
	}
}

func TestStartupMigrations_NoRepos(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	if err := runStartupMigrations(ctx, d, vault, logger); err != nil {
		t.Fatalf("migration on empty db: %v", err)
	}
}

func TestStartupMigrations_PartialPreExisting(t *testing.T) {
	ctx := context.Background()
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	d := newTestSpindleDB(t)
	vault := newTestVault(t)

	owner := "did:plc:akshay"
	repoDid := "did:plc:boltless"
	rkey := "3kspindlerkey00a"
	mustAddRepo(t, d, "knot.test", owner, rkey, repoDid)

	created := time.Date(2024, 6, 1, 12, 0, 0, 0, time.UTC)
	oldKey := owner + "/" + rkey
	mustAddSecret(t, vault, oldKey, "API_KEY", "alpha", created, owner)
	mustAddSecret(t, vault, oldKey, "DB_PASSWORD", "bravo", created, owner)

	mustAddSecret(t, vault, repoDid, "API_KEY", "pre-existing", created.Add(-24*time.Hour), owner)

	if err := runStartupMigrations(ctx, d, vault, logger); err != nil {
		t.Fatalf("migration: %v", err)
	}

	got, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(repoDid))
	if err != nil {
		t.Fatalf("GetSecretsUnlocked: %v", err)
	}
	if len(got) != 2 {
		t.Fatalf("expected 2 secrets under new key, got %d", len(got))
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
