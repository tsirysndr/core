package knotstream

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/rand"
	"net/http"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/util/ssrf"
	"github.com/carlmjohnson/versioninfo"
	"github.com/gorilla/websocket"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/log"
)

type KnotSlurper struct {
	logger *slog.Logger
	db     *sql.DB
	cfg    config.SlurperConfig

	subsLk sync.Mutex
	subs   map[string]*subscription
}

func NewKnotSlurper(l *slog.Logger, db *sql.DB, cfg config.SlurperConfig) *KnotSlurper {
	return &KnotSlurper{
		logger: log.SubLogger(l, "slurper"),
		db:     db,
		cfg:    cfg,
		subs:   make(map[string]*subscription),
	}
}

func (s *KnotSlurper) Run(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(s.cfg.PersistCursorPeriod):
			if err := s.persistCursors(ctx); err != nil {
				s.logger.Error("failed to flush cursors", "err", err)
			}
		}
	}
}

func (s *KnotSlurper) CheckIfSubscribed(hostname string) bool {
	s.subsLk.Lock()
	defer s.subsLk.Unlock()

	_, ok := s.subs[hostname]
	return ok
}

func (s *KnotSlurper) Shutdown(ctx context.Context) error {
	s.logger.Info("starting shutdown host cursor flush")
	err := s.persistCursors(ctx)
	if err != nil {
		s.logger.Error("shutdown error", "err", err)
	}
	s.logger.Info("slurper shutdown complete")
	return err
}

func (s *KnotSlurper) persistCursors(ctx context.Context) error {
	// // gather cursor list from subscriptions and store them to DB
	// start := time.Now()

	s.subsLk.Lock()
	cursors := make([]models.HostCursor, len(s.subs))
	i := 0
	for _, sub := range s.subs {
		cursors[i] = sub.HostCursor()
		i++
	}
	s.subsLk.Unlock()

	err := db.StoreCursors(ctx, s.db, cursors)
	// s.logger.Info("finished persisting cursors", "count", len(cursors), "duration", time.Since(start).String(), "err", err)
	return err
}

func (s *KnotSlurper) Subscribe(ctx context.Context, host models.Host) error {
	s.subsLk.Lock()
	defer s.subsLk.Unlock()

	_, ok := s.subs[host.Hostname]
	if ok {
		return fmt.Errorf("already subscribed: %s", host.Hostname)
	}

	// TODO: include `cancel` function to kill subscription by hostname
	sub := &subscription{
		hostname: host.Hostname,
		scheduler: NewParallelScheduler(
			s.cfg.ConcurrencyPerHost,
			host.Hostname,
			s.ProcessEvent,
		),
	}
	s.subs[host.Hostname] = sub

	sub.scheduler.Start(ctx)
	go s.subscribeWithRedialer(ctx, host, sub)
	return nil
}

func (s *KnotSlurper) subscribeWithRedialer(ctx context.Context, host models.Host, sub *subscription) {
	l := s.logger.With("host", host.Hostname)

	dialer := websocket.Dialer{
		HandshakeTimeout: time.Second * 5,
	}

	// if this isn't a localhost / private connection, then we should enable SSRF protections
	if !host.NoSSL {
		netDialer := ssrf.PublicOnlyDialer()
		dialer.NetDialContext = netDialer.DialContext
	}

	cursor := host.LastSeq

	connectedInbound.Inc()
	defer connectedInbound.Dec()

	var backoff int
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		u := host.LegacyEventsURL(cursor)
		l.Debug("made url with cursor", "cursor", cursor, "url", u)

		// NOTE: manual backoff retry implementation to explicitly handle fails
		hdr := make(http.Header)
		hdr.Add("User-Agent", userAgent())
		conn, resp, err := dialer.DialContext(ctx, u, hdr)
		if err != nil {
			l.Warn("dialing failed", "err", err, "backoff", backoff)
			time.Sleep(sleepForBackoff(backoff))
			backoff++
			if backoff > 30 {
				l.Warn("host does not appear to be online, disabling for now")
				host.Status = models.HostStatusOffline
				if err := db.UpsertHost(ctx, s.db, &host); err != nil {
					l.Error("failed to update host status", "err", err)
				}
				return
			}
			continue
		}

		l.Debug("knot event subscription response", "code", resp.StatusCode, "url", u)

		if err := s.handleConnection(ctx, conn, sub); err != nil {
			// TODO: measure the last N connection error times and if they're coming too fast reconnect slower or don't reconnect and wait for requestCrawl
			l.Warn("host connection failed", "err", err, "backoff", backoff)
		}

		updatedCursor := sub.LastSeq()
		didProgress := updatedCursor > cursor
		l.Debug("cursor compare", "cursor", cursor, "updatedCursor", updatedCursor, "didProgress", didProgress)
		if cursor == 0 || didProgress {
			cursor = updatedCursor
			backoff = 0

			batch := []models.HostCursor{sub.HostCursor()}
			if err := db.StoreCursors(ctx, s.db, batch); err != nil {
				l.Error("failed to store cursors", "err", err)
			}
		}
	}
}

// handleConnection handles websocket connection.
// Schedules task from received event and return when connection is closed
func (s *KnotSlurper) handleConnection(ctx context.Context, conn *websocket.Conn, sub *subscription) error {
	// ping on every 30s
	ctx, cancel := context.WithCancel(ctx)
	defer cancel() // close the background ping job on connection close
	go func() {
		t := time.NewTicker(30 * time.Second)
		defer t.Stop()
		failcount := 0

		for {
			select {
			case <-t.C:
				if err := conn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(time.Second*10)); err != nil {
					s.logger.Warn("failed to ping", "err", err)
					failcount++
					if failcount >= 4 {
						s.logger.Error("too many ping fails", "count", failcount)
						_ = conn.Close()
						return
					}
				} else {
					failcount = 0 // ok ping
				}
			case <-ctx.Done():
				_ = conn.Close()
				return
			}
		}
	}()

	conn.SetPingHandler(func(message string) error {
		err := conn.WriteControl(websocket.PongMessage, []byte(message), time.Now().Add(time.Minute))
		if err == websocket.ErrCloseSent {
			return nil
		}
		return err
	})
	conn.SetPongHandler(func(_ string) error {
		if err := conn.SetReadDeadline(time.Now().Add(time.Minute)); err != nil {
			s.logger.Error("failed to set read deadline", "err", err)
		}
		return nil
	})

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		msgType, msg, err := conn.ReadMessage()
		if err != nil {
			return err
		}

		if msgType != websocket.TextMessage {
			continue
		}

		sub.scheduler.AddTask(ctx, &Task{
			key:     sub.hostname, // TODO: replace to repository AT-URI for better concurrency
			message: msg,
		})
	}
}

type LegacyGitEvent struct {
	Rkey  string
	Nsid  string
	Event tangled.GitRefUpdate
}

func (s *KnotSlurper) ProcessEvent(ctx context.Context, task *Task) error {
	var legacyMessage LegacyGitEvent
	if err := json.Unmarshal(task.message, &legacyMessage); err != nil {
		return fmt.Errorf("unmarshaling message: %w", err)
	}

	if err := s.ProcessLegacyGitRefUpdate(ctx, task.key, &legacyMessage); err != nil {
		return fmt.Errorf("processing gitRefUpdate: %w", err)
	}
	return nil
}

func (s *KnotSlurper) ProcessLegacyGitRefUpdate(ctx context.Context, source string, evt *LegacyGitEvent) error {
	knotstreamEventsReceived.Inc()

	l := s.logger.With("src", source)

	repoDid := ""
	if evt.Event.RepoDid != nil {
		repoDid = *evt.Event.RepoDid
	}
	curr, err := db.GetRepoByName(ctx, s.db, syntax.DID(repoDid), evt.Event.RepoName)
	if err != nil {
		return fmt.Errorf("failed to get repo '%s': %w", repoDid+"/"+evt.Event.RepoName, err)
	}
	if curr == nil {
		// if repo doesn't exist in DB, just ignore the event. That repo is unknown.
		//
		// Normally did+name is already enough to perform git-fetch as that's
		// what needed to fetch the repository.
		// But we want to store that in did/rkey in knot-mirror.
		// Therefore, we should ignore when the repository is unknown.
		// Hopefully crawler will sync it later.
		l.Warn("skipping event from unknown repo", "did/name", repoDid+"/"+evt.Event.RepoName)
		knotstreamEventsSkipped.Inc()
		return nil
	}
	l = l.With("repoAt", curr.AtUri())

	// TODO: should plan resync to resyncBuffer on RepoStateResyncing
	if curr.State != models.RepoStateActive {
		l.Debug("skipping non-active repo")
		knotstreamEventsSkipped.Inc()
		return nil
	}

	if curr.GitRev != "" && evt.Rkey <= curr.GitRev.String() {
		l.Debug("skipping replayed event", "event.Rkey", evt.Rkey, "currentRev", curr.GitRev)
		knotstreamEventsSkipped.Inc()
		return nil
	}

	// if curr.State == models.RepoStateResyncing {
	// 	firehoseEventsSkipped.Inc()
	// 	return fp.events.addToResyncBuffer(ctx, commit)
	// }

	// can't skip anything, update repo state
	if err := db.UpdateRepoState(ctx, s.db, curr.Did, curr.Rkey, models.RepoStateDesynchronized); err != nil {
		return err
	}

	l.Info("event processed", "eventRev", evt.Rkey)

	knotstreamEventsProcessed.Inc()
	return nil
}

func userAgent() string {
	return fmt.Sprintf("knotmirror/%s", versioninfo.Short())
}

func sleepForBackoff(b int) time.Duration {
	if b == 0 {
		return 0
	}
	if b < 10 {
		return time.Millisecond * time.Duration((50*b)+rand.Intn(500))
	}
	return time.Second * 30
}
