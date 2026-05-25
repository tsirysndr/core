package spindle

import (
	"context"
	"database/sql"
	"fmt"
	"net/url"
	"sync/atomic"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/events"
	"github.com/bluesky-social/indigo/events/schedulers/sequential"
	"github.com/gorilla/websocket"
	_ "github.com/mattn/go-sqlite3"
)

const (
	tapDrainPollInterval = 3 * time.Second
	tapDrainStableChecks = 2
	tapEmptyGraceChecks  = 10
)

func (s *Spindle) watchTapDrain(ctx context.Context, stop context.CancelFunc) {
	headSeq, err := relayHeadSeq(ctx, s.cfg.Server.Tap.RelayUrl)
	if err != nil {
		s.l.Warn("tap drain watcher: relay head checking failed, falling back to resync-drain only", "err", err)
		headSeq = 0
	} else {
		s.l.Info("tap drain watcher: relay head seq at startup", "head", headSeq, "relay", s.cfg.Server.Tap.RelayUrl)
	}

	conn, err := sql.Open("sqlite3", s.cfg.Server.Tap.DBPath)
	if err != nil {
		s.l.Warn("tap drain watcher: opening tap db failed", "err", err)
		return
	}
	defer conn.Close()

	ticker := time.NewTicker(tapDrainPollInterval)
	defer ticker.Stop()

	sawWork := false
	readyStreak := 0
	emptyStreak := 0
	queryFailed := false

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			var total, busy int
			if err := conn.QueryRowContext(ctx, `
				select count(*),
				       coalesce(sum(case when state in ('pending','resyncing','desynchronized') then 1 else 0 end), 0)
				from repos`).Scan(&total, &busy); err != nil {
				if !queryFailed {
					s.l.Warn("tap drain watcher: repos query failed", "err", err)
					queryFailed = true
				}
				continue
			}
			queryFailed = false

			var cursor int64
			if headSeq > 0 {
				if err := conn.QueryRowContext(ctx,
					`select cursor from firehose_cursors where url = ?`,
					s.cfg.Server.Tap.RelayUrl,
				).Scan(&cursor); err != nil {
					cursor = 0
				}
			}

			if total > 0 {
				sawWork = true
				emptyStreak = 0
			} else {
				emptyStreak++
			}

			caughtUp := headSeq <= 0 || cursor >= headSeq
			drained := sawWork && busy == 0

			if caughtUp && drained {
				readyStreak++
			} else {
				readyStreak = 0
			}

			if readyStreak >= tapDrainStableChecks {
				s.l.Info("tap caught up and backfill drained, shutting down embedded tap!", "tracked", total, "cursor", cursor, "head", headSeq)
				stop()
				s.embedTap.Shutdown()
				return
			}
			if !sawWork && emptyStreak >= tapEmptyGraceChecks {
				s.l.Info("tap has nothing to backfill, shutting down embedded tap!")
				stop()
				s.embedTap.Shutdown()
				return
			}
		}
	}
}

func relayHeadSeq(ctx context.Context, relayURL string) (int64, error) {
	u, err := url.Parse(relayURL)
	if err != nil {
		return 0, err
	}
	switch u.Scheme {
	case "http":
		u.Scheme = "ws"
	case "https":
		u.Scheme = "wss"
	}
	u.Path = "xrpc/com.atproto.sync.subscribeRepos"

	dialCtx, cancelDial := context.WithTimeout(ctx, 15*time.Second)
	defer cancelDial()

	conn, _, err := websocket.DefaultDialer.DialContext(dialCtx, u.String(), nil)
	if err != nil {
		return 0, fmt.Errorf("dial relay: %w", err)
	}
	defer conn.Close()

	streamCtx, cancelStream := context.WithCancel(dialCtx)
	defer cancelStream()

	var seq atomic.Int64
	capture := func(v int64) error {
		seq.Store(v)
		cancelStream()
		return nil
	}
	rsc := &events.RepoStreamCallbacks{
		RepoCommit:   func(e *comatproto.SyncSubscribeRepos_Commit) error { return capture(e.Seq) },
		RepoSync:     func(e *comatproto.SyncSubscribeRepos_Sync) error { return capture(e.Seq) },
		RepoIdentity: func(e *comatproto.SyncSubscribeRepos_Identity) error { return capture(e.Seq) },
		RepoAccount:  func(e *comatproto.SyncSubscribeRepos_Account) error { return capture(e.Seq) },
	}
	sched := sequential.NewScheduler("spindle-head-probe", rsc.EventHandler)
	_ = events.HandleRepoStream(streamCtx, conn, sched, nil)

	if h := seq.Load(); h > 0 {
		return h, nil
	}
	return 0, fmt.Errorf("no head seq received from relay")
}
