package migration

import (
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
)

const maxConcurrentMigrations = 8

type Migration struct {
	db       *db.DB
	oauth    *oauth.OAuth
	dir      identity.Directory
	logger   *slog.Logger
	inflight sync.Map
	sem      chan struct{}
}

func NewMigration(db *db.DB, oauth *oauth.OAuth, dir identity.Directory, logger *slog.Logger) *Migration {
	return &Migration{
		db:     db,
		oauth:  oauth,
		dir:    dir,
		logger: logger,
		sem:    make(chan struct{}, maxConcurrentMigrations),
	}
}

func (s *Migration) BackgroundMigrationMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer next.ServeHTTP(w, r)

		did := s.oauth.GetDidFromCookie(r)
		if did == "" {
			return
		}

		hasPending, err := db.HasPendingPdsRecordMigration(r.Context(), s.db, did)
		if err != nil || !hasPending {
			return
		}

		if _, loaded := s.inflight.LoadOrStore(did, struct{}{}); loaded {
			return
		}

		select {
		case s.sem <- struct{}{}:
		default:
			s.inflight.Delete(did)
			return
		}

		client, err := s.oauth.AuthorizedClient(r)
		if err != nil || client.AccountDID == nil {
			<-s.sem
			s.inflight.Delete(did)
			return
		}

		go func() {
			defer s.inflight.Delete(did)
			defer func() { <-s.sem }()
			s.runPendingMigrations(context.Background(), *client.AccountDID, client)
		}()
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
