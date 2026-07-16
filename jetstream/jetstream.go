package jetstream

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/bluesky-social/jetstream/pkg/client"
	"github.com/bluesky-social/jetstream/pkg/client/schedulers/sequential"
	"github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/log"
)

type DB interface {
	GetLastTimeUs() (int64, error)
	SaveLastTimeUs(int64) error
}

type Set[T comparable] map[T]struct{}

type JetstreamClient struct {
	cfg    *client.ClientConfig
	client *client.Client
	ident  string
	l      *slog.Logger

	logDids           bool
	wantedDids        Set[string]
	unfilteredNsids   Set[string]
	db         DB
	waitForDid bool
	mu         sync.RWMutex

	lastSeenUs atomic.Int64

	cancel   context.CancelFunc
	cancelMu sync.Mutex
}

func (j *JetstreamClient) AddDid(did string) {
	if did == "" {
		return
	}

	if j.logDids {
		j.l.Info("adding did to in-memory filter", "did", did)
	}
	j.mu.Lock()
	j.wantedDids[did] = struct{}{}
	j.mu.Unlock()
}

func (j *JetstreamClient) ExemptCollection(nsid string) {
	j.mu.Lock()
	j.unfilteredNsids[nsid] = struct{}{}
	j.mu.Unlock()
}

func (j *JetstreamClient) RemoveDid(did string) {
	if did == "" {
		return
	}

	if j.logDids {
		j.l.Info("removing did from in-memory filter", "did", did)
	}
	j.mu.Lock()
	delete(j.wantedDids, did)
	j.mu.Unlock()
}

type processor func(context.Context, *models.Event) error

func (j *JetstreamClient) withDidFilter(processFunc processor) processor {
	// since this closure references j.WantedDids; it should auto-update
	// existing instances of the closure when j.WantedDids is mutated
	return func(ctx context.Context, evt *models.Event) error {
		j.mu.RLock()
		// empty filter => all dids allowed
		matches := len(j.wantedDids) == 0
		if !matches {
			if _, ok := j.wantedDids[evt.Did]; ok {
				matches = true
			}
		}
		if !matches && evt.Commit != nil {
			if _, ok := j.unfilteredNsids[evt.Commit.Collection]; ok {
				matches = true
			}
		}
		j.mu.RUnlock()

		var err error
		if matches {
			err = processFunc(ctx, evt)
		}

		j.lastSeenUs.Store(evt.TimeUS + 1)
		return err
	}
}

func NewJetstreamClient(endpoint, ident string, collections []string, cfg *client.ClientConfig, logger *slog.Logger, db DB, waitForDid, logDids bool) (*JetstreamClient, error) {
	if cfg == nil {
		cfg = client.DefaultClientConfig()
		cfg.WebsocketURL = endpoint
		cfg.WantedCollections = collections
	}

	return &JetstreamClient{
		cfg:        cfg,
		ident:      ident,
		db:         db,
		l:          logger,
		wantedDids:      make(map[string]struct{}),
		unfilteredNsids: make(map[string]struct{}),

		logDids: logDids,

		// This will make the goroutine in StartJetstream wait until
		// j.wantedDids has been populated, typically using addDids.
		waitForDid: waitForDid,
	}, nil
}

// StartJetstream starts the jetstream client and processes events using the provided processFunc.
// The client persists the last time_us cursor itself via the DB it was constructed with.
func (j *JetstreamClient) StartJetstream(ctx context.Context, processFunc func(context.Context, *models.Event) error) error {
	logger := j.l

	sched := sequential.NewScheduler(j.ident, logger, j.withDidFilter(processFunc))

	client, err := client.NewClient(j.cfg, logger, sched)
	if err != nil {
		return fmt.Errorf("failed to create jetstream client: %w", err)
	}
	j.client = client

	go func() {
		if j.waitForDid {
			for {
				j.mu.RLock()
				hasDid := len(j.wantedDids) != 0
				j.mu.RUnlock()
				if hasDid {
					break
				}
				time.Sleep(time.Second)
			}
		}
		logger.Info("done waiting for did")

		go j.periodicLastTimeSave(ctx)
		j.saveIfKilled(ctx)

		j.connectAndRead(ctx)
	}()

	return nil
}

func (j *JetstreamClient) connectAndRead(ctx context.Context) {
	l := log.FromContext(ctx)
	for {
		cursor := j.resumeCursor(ctx)

		connCtx, cancel := context.WithCancel(ctx)
		j.cancelMu.Lock()
		j.cancel = cancel
		j.cancelMu.Unlock()

		if err := j.client.ConnectAndRead(connCtx, cursor); err != nil {
			l.Error("error reading jetstream", "error", err)
			cancel()
			continue
		}

		select {
		case <-ctx.Done():
			l.Info("context done, stopping jetstream")
			return
		case <-connCtx.Done():
			l.Info("connection context done, reconnecting")
			continue
		}
	}
}

// save cursor periodically
func (j *JetstreamClient) periodicLastTimeSave(ctx context.Context) {
	ticker := time.NewTicker(time.Minute)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if seen := j.lastSeenUs.Load(); seen != 0 {
				if err := j.db.SaveLastTimeUs(seen); err != nil {
					log.FromContext(ctx).Error("failed to save cursor", "error", err)
				}
			}
		}
	}
}

func (j *JetstreamClient) resumeCursor(ctx context.Context) *int64 {
	if seen := j.lastSeenUs.Load(); seen != 0 {
		return &seen
	}
	return j.getLastTimeUs(ctx)
}

func (j *JetstreamClient) getLastTimeUs(ctx context.Context) *int64 {
	l := log.FromContext(ctx)
	lastTimeUs, err := j.db.GetLastTimeUs()
	if err != nil {
		l.Warn("couldn't get last time us, starting from now", "error", err)
		lastTimeUs = time.Now().UnixMicro()
		if err = j.db.SaveLastTimeUs(lastTimeUs); err != nil {
			l.Error("failed to save last time us", "error", err)
		}
	}

	l.Info("found last time_us", "time_us", lastTimeUs)
	return &lastTimeUs
}

func (j *JetstreamClient) saveIfKilled(ctx context.Context) context.Context {
	ctxWithCancel, cancel := context.WithCancel(ctx)

	sigChan := make(chan os.Signal, 1)

	signal.Notify(sigChan,
		syscall.SIGINT,
		syscall.SIGTERM,
		syscall.SIGQUIT,
		syscall.SIGHUP,
		syscall.SIGKILL,
		syscall.SIGSTOP,
	)

	go func() {
		sig := <-sigChan
		j.l.Info("Received signal, initiating graceful shutdown", "signal", sig)

		if seen := j.lastSeenUs.Load(); seen != 0 {
			if err := j.db.SaveLastTimeUs(seen); err != nil {
				j.l.Error("Failed to save last time during shutdown", "error", err)
			}
			j.l.Info("Saved lastTimeUs before shutdown", "lastTimeUs", seen)
		}

		j.cancelMu.Lock()
		if j.cancel != nil {
			j.cancel()
		}
		j.cancelMu.Unlock()

		cancel()

		os.Exit(0)
	}()

	return ctxWithCancel
}
