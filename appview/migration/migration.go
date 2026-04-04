package migration

import (
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
)

type Migration struct {
	db     *db.DB
	oauth  *oauth.OAuth
	dir    identity.Directory
	logger *slog.Logger
}

func NewMigration(db *db.DB, oauth *oauth.OAuth, dir identity.Directory, logger *slog.Logger) *Migration {
	return &Migration{
		db, oauth, dir, logger,
	}
}

func (s *Migration) BackgroundMigrationMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer next.ServeHTTP(w, r)

		client, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			return
		}
		if client.AccountDID == nil {
			return
		}

		go s.runPendingMigrations(context.Background(), *client.AccountDID, client)
	})
}

func (s *Migration) runPendingMigrations(ctx context.Context, did syntax.DID, client *atclient.APIClient) {
	l := s.logger.With("did", did)
	migrations, err := db.ListPendingPdsRecordMigrations(ctx, s.db, did)
	if err != nil {
		l.Error("failed to query pending migrations", "err", err)
		return
	}

	for _, migration := range migrations {
		if err := s.migrate(ctx, client, migration); err != nil {
			l.Error("migration failed", "err", err)
		}
	}
}

func (s *Migration) migrate(ctx context.Context, client *atclient.APIClient, migration *models.PDSMigration) error {
	l := s.logger.With(
		"name", migration.Name,
		"aturi", migration.RecordAtUri(),
	)

	var err error
	switch migration.Name {
	case "add-repo-did":
		err = s.migrateAddRepoDid(ctx, client, migration.Did, migration.RecordAtUri())
	default:
		return fmt.Errorf("unexpected migration name %s", migration.Name)
	}

	if err == nil {
		l.Info("migrated")
		migration.Status = models.PDSMigrationStatusDone
	} else {
		l.Warn("failed to migrate", "err", err)

		errMsg := err.Error()
		var retryCount = migration.RetryCount + 1
		var retryAfter = time.Now().Add(3 * time.Second).Unix()

		// remove null bytes
		errMsg = strings.ReplaceAll(errMsg, "\x00", "")

		migration.Status = models.PDSMigrationStatusPending
		migration.ErrorMsg = &errMsg
		migration.RetryCount = retryCount
		migration.RetryAfter = retryAfter
	}
	if err := db.UpdatePdsRecordMigration(ctx, s.db, migration); err != nil {
		return fmt.Errorf("failed to update migration status: %w", err)
	}
	return nil
}
