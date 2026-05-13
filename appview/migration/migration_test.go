package migration

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"path/filepath"
	"sync/atomic"
	"testing"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
)

func newTestDB(t *testing.T) *db.DB {
	t.Helper()
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func seedMigration(t *testing.T, d *db.DB, mig *models.PDSMigration) {
	t.Helper()
	if err := db.EnqueuePdsRecordMigration(context.Background(), d, mig.Name, mig.Did, mig.Collection, mig.Rkey); err != nil {
		t.Fatalf("EnqueuePdsRecordMigration: %v", err)
	}
}

func fetch(t *testing.T, d *db.DB, did syntax.DID) *models.PDSMigration {
	t.Helper()
	rows, err := d.QueryContext(context.Background(),
		`select name, did, collection, rkey, status, error_msg, retry_count, retry_after from pds_migration where did = ?`, did)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	defer rows.Close()
	if !rows.Next() {
		t.Fatalf("no row for did %s", did)
	}
	var m models.PDSMigration
	if err := rows.Scan(&m.Name, &m.Did, &m.Collection, &m.Rkey, &m.Status, &m.ErrorMsg, &m.RetryCount, &m.RetryAfter); err != nil {
		t.Fatalf("scan: %v", err)
	}
	return &m
}

func newTestMigration(t *testing.T, mig migrator, onPerm permAuthErrHandler) *Migration {
	t.Helper()
	return &Migration{
		db:            newTestDB(t),
		logger:        slog.New(slog.NewTextHandler(io.Discard, nil)),
		sem:           make(chan struct{}, maxConcurrentMigrations),
		migrators:     map[string]migrator{"add-repo-did": mig},
		onPermAuthErr: onPerm,
	}
}

func newPDSMigration(did syntax.DID) *models.PDSMigration {
	return &models.PDSMigration{
		Name:       "add-repo-did",
		Did:        did,
		Collection: "sh.tangled.repo",
		Rkey:       "abc",
		Status:     models.PDSMigrationStatusRunning,
	}
}

func TestMigrateInvalidGrantMarksFailed(t *testing.T) {
	did := syntax.DID("did:plc:boltless")
	var permCalled atomic.Int32
	m := newTestMigration(t,
		func(context.Context, *atclient.APIClient, syntax.DID, syntax.ATURI) error {
			return errors.New("put record: failed to refresh OAuth tokens: token refresh failed: auth server request failed (HTTP 400): invalid_grant")
		},
		func(_ context.Context, _ syntax.DID, _ string, err error) bool {
			permCalled.Add(1)
			return oauth.IsPermanentAuthErr(err)
		},
	)
	seedMigration(t, m.db, newPDSMigration(did))
	pm := newPDSMigration(did)

	if err := m.migrate(context.Background(), &atclient.APIClient{}, "sess1", pm); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	got := fetch(t, m.db, did)
	if got.Status != models.PDSMigrationStatusFailed {
		t.Fatalf("status = %s, want failed", got.Status)
	}
	if got.RetryAfter != 0 {
		t.Fatalf("RetryAfter = %d, want 0", got.RetryAfter)
	}
	if permCalled.Load() != 1 {
		t.Fatalf("onPermAuthErr called %d times, want 1", permCalled.Load())
	}
}

func TestMigrateTransientErrorStaysPending(t *testing.T) {
	did := syntax.DID("did:plc:akshay")
	m := newTestMigration(t,
		func(context.Context, *atclient.APIClient, syntax.DID, syntax.ATURI) error {
			return errors.New("put record: failed to refresh OAuth tokens: token refresh failed (HTTP 429): rate_limited")
		},
		func(_ context.Context, _ syntax.DID, _ string, err error) bool {
			return oauth.IsPermanentAuthErr(err)
		},
	)
	seedMigration(t, m.db, newPDSMigration(did))
	pm := newPDSMigration(did)

	if err := m.migrate(context.Background(), &atclient.APIClient{}, "sess1", pm); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	got := fetch(t, m.db, did)
	if got.Status != models.PDSMigrationStatusPending {
		t.Fatalf("status = %s, want pending", got.Status)
	}
	if got.RetryCount != 1 {
		t.Fatalf("RetryCount = %d, want 1", got.RetryCount)
	}
	if got.RetryAfter == 0 {
		t.Fatalf("RetryAfter not scheduled")
	}
}

func TestMigrateSuccess(t *testing.T) {
	did := syntax.DID("did:plc:boltless")
	m := newTestMigration(t,
		func(context.Context, *atclient.APIClient, syntax.DID, syntax.ATURI) error { return nil },
		func(context.Context, syntax.DID, string, error) bool { return false },
	)
	seedMigration(t, m.db, newPDSMigration(did))
	pm := newPDSMigration(did)

	if err := m.migrate(context.Background(), &atclient.APIClient{}, "sess1", pm); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	got := fetch(t, m.db, did)
	if got.Status != models.PDSMigrationStatusDone {
		t.Fatalf("status = %s, want done", got.Status)
	}
}

func TestEnqueueResetsFailedToPending(t *testing.T) {
	d := newTestDB(t)
	did := syntax.DID("did:plc:boltless")
	seed := newPDSMigration(did)
	seedMigration(t, d, seed)

	errMsg := "some prior failure"
	failed := *seed
	failed.Status = models.PDSMigrationStatusFailed
	failed.RetryCount = 7
	failed.ErrorMsg = &errMsg
	if err := db.UpdatePdsRecordMigration(context.Background(), d, &failed); err != nil {
		t.Fatalf("UpdatePdsRecordMigration: %v", err)
	}

	if err := db.EnqueuePdsRecordMigration(context.Background(), d, seed.Name, seed.Did, seed.Collection, seed.Rkey); err != nil {
		t.Fatalf("re-enqueue: %v", err)
	}

	got := fetch(t, d, did)
	if got.Status != models.PDSMigrationStatusPending {
		t.Fatalf("status = %s, want pending", got.Status)
	}
	if got.RetryCount != 0 {
		t.Fatalf("RetryCount = %d, want 0", got.RetryCount)
	}
	if got.ErrorMsg != nil {
		t.Fatalf("ErrorMsg = %v, want nil", got.ErrorMsg)
	}
}

func TestReapStaleRunning(t *testing.T) {
	d := newTestDB(t)
	did := syntax.DID("did:plc:akshay")
	seed := newPDSMigration(did)
	seedMigration(t, d, seed)
	running := *seed
	running.Status = models.PDSMigrationStatusRunning
	if err := db.UpdatePdsRecordMigration(context.Background(), d, &running); err != nil {
		t.Fatalf("update: %v", err)
	}

	if err := db.ReapStaleRunningMigrations(context.Background(), d); err != nil {
		t.Fatalf("reap: %v", err)
	}

	got := fetch(t, d, did)
	if got.Status != models.PDSMigrationStatusPending {
		t.Fatalf("status = %s, want pending", got.Status)
	}
}
