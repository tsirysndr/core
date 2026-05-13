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

type migrator func(ctx context.Context, client *atclient.APIClient, did syntax.DID, aturi syntax.ATURI) error

type permAuthErrHandler func(ctx context.Context, did syntax.DID, sessId string, err error) bool

type Migration struct {
	db            *db.DB
	oauth         *oauth.OAuth
	dir           identity.Directory
	logger        *slog.Logger
	inflight      sync.Map
	sem           chan struct{}
	migrators     map[string]migrator
	onPermAuthErr permAuthErrHandler
}

func NewMigration(db *db.DB, oauth *oauth.OAuth, dir identity.Directory, logger *slog.Logger) *Migration {
	m := &Migration{
		db:            db,
		oauth:         oauth,
		dir:           dir,
		logger:        logger,
		sem:           make(chan struct{}, maxConcurrentMigrations),
		onPermAuthErr: oauth.HandlePermanentAuthErr,
	}
	m.migrators = map[string]migrator{
		"add-repo-did": m.migrateAddRepoDid,
	}
	return m
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

		sessId := s.oauth.GetSessIdFromCookie(r)
		client, err := s.oauth.AuthorizedClient(r)
		if err != nil || client.AccountDID == nil {
			<-s.sem
			s.inflight.Delete(did)
			return
		}

		go func() {
			defer s.inflight.Delete(did)
			defer func() { <-s.sem }()
			s.runPendingMigrations(context.Background(), *client.AccountDID, sessId, client)
		}()
	})
}

func (s *Migration) runPendingMigrations(ctx context.Context, did syntax.DID, sessId string, client *atclient.APIClient) {
	l := s.logger.With("did", did)
	migrations, err := db.ListPendingPdsRecordMigrations(ctx, s.db, did)
	if err != nil {
		l.Error("failed to query pending migrations", "err", err)
		return
	}

	for _, migration := range migrations {
		if err := s.migrate(ctx, client, sessId, migration); err != nil {
			l.Error("migration failed", "err", err)
		}
	}
}

func (s *Migration) migrate(ctx context.Context, client *atclient.APIClient, sessId string, migration *models.PDSMigration) error {
	l := s.logger.With(
		"name", migration.Name,
		"aturi", migration.RecordAtUri(),
	)

	mig, ok := s.migrators[migration.Name]
	if !ok {
		return fmt.Errorf("unexpected migration name %s", migration.Name)
	}
	err := mig(ctx, client, migration.Did, migration.RecordAtUri())

	if err == nil {
		l.Info("migrated")
		migration.Status = models.PDSMigrationStatusDone
	} else {
		l.Warn("failed to migrate", "err", err)

		errMsg := strings.ReplaceAll(err.Error(), "\x00", "")
		migration.ErrorMsg = &errMsg
		migration.RetryCount++

		if s.onPermAuthErr(ctx, migration.Did, sessId, err) {
			migration.Status = models.PDSMigrationStatusFailed
			migration.RetryAfter = 0
		} else {
			migration.Status = models.PDSMigrationStatusPending
			migration.RetryAfter = time.Now().Add(retryBackoff(migration.RetryCount)).Unix()
		}
	}
	if err := db.UpdatePdsRecordMigration(ctx, s.db, migration); err != nil {
		return fmt.Errorf("failed to update migration status: %w", err)
	}
	return nil
}

func retryBackoff(retries int) time.Duration {
	return min(time.Duration(retries)*5*time.Second, time.Hour)
}
