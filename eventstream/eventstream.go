package eventstream

import (
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"strconv"
	"time"

	"github.com/gorilla/websocket"
	"tangled.org/core/notifier"
)

type Event struct {
	Rkey      string          `json:"rkey"`
	Nsid      string          `json:"nsid"`
	EventJson json.RawMessage `json:"event"`
	Created   int64           `json:"created"`
}

type Backend interface {
	GetEvents(cursor int64, limit int) ([]Event, error)
}

const (
	defaultBatchSize          = 100
	defaultMaxBatchesPerDrain = 1_000
	keepAliveInterval         = 30 * time.Second
	writeDeadline             = 10 * time.Second
)

var ErrDrainCap = errors.New("eventstream: drain cap reached, reconnect to continue")

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

type StreamConfig struct {
	Backend  Backend
	Notifier *notifier.Notifier
	Logger   *slog.Logger

	BatchSize          int
	MaxBatchesPerDrain int
}

func (c *StreamConfig) batchSize() int {
	if c.BatchSize > 0 {
		return c.BatchSize
	}
	return defaultBatchSize
}

func (c *StreamConfig) maxBatchesPerDrain() int {
	if c.MaxBatchesPerDrain > 0 {
		return c.MaxBatchesPerDrain
	}
	return defaultMaxBatchesPerDrain
}

func Stream(w http.ResponseWriter, r *http.Request, cfg StreamConfig) error {
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return err
	}
	defer conn.Close()

	var cursor int64
	if raw := r.URL.Query().Get("cursor"); raw != "" {
		parsed, perr := strconv.ParseInt(raw, 10, 64)
		if perr != nil {
			if cfg.Logger != nil {
				cfg.Logger.Warn("invalid cursor, starting from head", "cursor", raw, "err", perr)
			}
		} else {
			cursor = parsed
		}
	}

	ch := cfg.Notifier.Subscribe()
	defer cfg.Notifier.Unsubscribe(ch)

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	go func() {
		for {
			if _, _, err := conn.NextReader(); err != nil {
				cancel()
				return
			}
		}
	}()

	drain := func() error {
		err := drainUntilShort(conn, cfg, &cursor)
		if errors.Is(err, ErrDrainCap) {
			_ = conn.WriteControl(
				websocket.CloseMessage,
				websocket.FormatCloseMessage(websocket.CloseTryAgainLater, "drain cap reached, reconnect to continue"),
				time.Now().Add(writeDeadline),
			)
		}
		return err
	}

	if err := drain(); err != nil {
		return err
	}

	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ch:
			if err := drain(); err != nil {
				return err
			}
		case <-time.After(keepAliveInterval):
			if err := conn.WriteControl(websocket.PingMessage, nil, time.Now().Add(writeDeadline)); err != nil {
				return err
			}
		}
	}
}

func drainUntilShort(conn *websocket.Conn, cfg StreamConfig, cursor *int64) error {
	limit := cfg.batchSize()
	for range cfg.maxBatchesPerDrain() {
		n, err := streamBatch(conn, cfg, cursor)
		if err != nil {
			return err
		}
		if n < limit {
			return nil
		}
	}
	if cfg.Logger != nil {
		cfg.Logger.Warn("drain hit batch cap", "cursor", *cursor, "cap", cfg.maxBatchesPerDrain())
	}
	return ErrDrainCap
}

func streamBatch(conn *websocket.Conn, cfg StreamConfig, cursor *int64) (int, error) {
	events, err := cfg.Backend.GetEvents(*cursor, cfg.batchSize())
	if err != nil {
		return 0, err
	}
	for _, ev := range events {
		msg, err := json.Marshal(ev)
		if err != nil {
			return 0, err
		}
		if err := conn.SetWriteDeadline(time.Now().Add(writeDeadline)); err != nil {
			return 0, err
		}
		if err := conn.WriteMessage(websocket.TextMessage, msg); err != nil {
			return 0, err
		}
		*cursor = ev.Created
	}
	return len(events), nil
}
