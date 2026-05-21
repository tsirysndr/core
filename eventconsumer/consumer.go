package eventconsumer

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/url"
	"sync"
	"time"

	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
	"tangled.org/core/log"

	"github.com/avast/retry-go/v4"
	"github.com/gorilla/websocket"
)

type ProcessFunc func(ctx context.Context, source Source, event eventstream.Event) error

type ConsumerConfig struct {
	Sources           map[Source]struct{}
	ProcessFunc       ProcessFunc
	RetryInterval     time.Duration
	MaxRetryInterval  time.Duration
	ConnectionTimeout time.Duration
	WorkerCount       int
	QueueSize         int
	Logger            *slog.Logger
	CursorStore       cursor.Store
	URLFunc           func(Source, int64) (*url.URL, error)

	Dialer            *websocket.Dialer
	RequestHeader     http.Header
	MaxRetryAttempts  uint
	OnConnectExceeded func(Source, error)
}

func NewConsumerConfig() *ConsumerConfig {
	return &ConsumerConfig{
		Sources: make(map[Source]struct{}),
	}
}

type Consumer struct {
	sourceWg sync.WaitGroup
	workerWg sync.WaitGroup
	dialer   *websocket.Dialer
	jobQueue chan job
	logger   *slog.Logger

	// sourcesMu guards sources. It must only be held for short, non-blocking
	// map operations; never across a blocking call (dial, read, close).
	sourcesMu sync.Mutex
	sources   map[Source]*sourceState

	cfg ConsumerConfig
}

type sourceState struct {
	cancel context.CancelFunc
	conn   *websocket.Conn

	cursorMu  sync.Mutex
	cursorMax int64
}

type job struct {
	source  Source
	message []byte
}

func NewConsumer(cfg ConsumerConfig) *Consumer {
	if cfg.RetryInterval == 0 {
		cfg.RetryInterval = 15 * time.Minute
	}
	if cfg.ConnectionTimeout == 0 {
		cfg.ConnectionTimeout = 10 * time.Second
	}
	if cfg.WorkerCount <= 0 {
		cfg.WorkerCount = 5
	}
	if cfg.MaxRetryInterval == 0 {
		cfg.MaxRetryInterval = 1 * time.Hour
	}
	if cfg.Logger == nil {
		cfg.Logger = log.New("consumer")
	}
	if cfg.QueueSize == 0 {
		cfg.QueueSize = 100
	}
	if cfg.CursorStore == nil {
		cfg.CursorStore = &cursor.MemoryStore{}
	}
	if cfg.URLFunc == nil {
		cfg.URLFunc = DefaultURL(false)
	}
	dialer := cfg.Dialer
	if dialer == nil {
		dialer = websocket.DefaultDialer
	}
	return &Consumer{
		cfg:      cfg,
		dialer:   dialer,
		jobQueue: make(chan job, cfg.QueueSize),
		logger:   cfg.Logger,
		sources:  make(map[Source]*sourceState),
	}
}

func (c *Consumer) Start(ctx context.Context) {
	c.cfg.Logger.Info("starting consumer", "config", c.cfg)

	for range c.cfg.WorkerCount {
		c.workerWg.Add(1)
		go c.worker(ctx)
	}

	for source := range c.cfg.Sources {
		c.AddSource(ctx, source)
	}
}

func (c *Consumer) Stop() {
	// snapshot cancels and conns under lock so we don't hold sourcesMu across Close
	c.sourcesMu.Lock()
	cancels := make([]context.CancelFunc, 0, len(c.sources))
	conns := make([]*websocket.Conn, 0, len(c.sources))
	for _, st := range c.sources {
		if st.cancel != nil {
			cancels = append(cancels, st.cancel)
		}
		if st.conn != nil {
			conns = append(conns, st.conn)
		}
	}
	c.sourcesMu.Unlock()

	for _, cancel := range cancels {
		cancel()
	}
	for _, conn := range conns {
		conn.Close()
	}

	c.sourceWg.Wait()
	close(c.jobQueue)
	c.workerWg.Wait()
}

func (c *Consumer) AddSource(ctx context.Context, s Source) {
	c.sourcesMu.Lock()
	if _, ok := c.sources[s]; ok {
		c.sourcesMu.Unlock()
		c.logger.Info("source already present", "source", s)
		return
	}
	srcCtx, cancel := context.WithCancel(ctx)
	c.sources[s] = &sourceState{cancel: cancel}
	c.sourcesMu.Unlock()

	c.sourceWg.Add(1)
	go c.startConnectionLoop(srcCtx, s)
}

func (c *Consumer) RemoveSource(s Source) {
	c.sourcesMu.Lock()
	st, ok := c.sources[s]
	if !ok {
		c.sourcesMu.Unlock()
		c.logger.Info("source not present", "source", s)
		return
	}
	delete(c.sources, s)
	cancel := st.cancel
	conn := st.conn
	c.sourcesMu.Unlock()

	// release lock before any potentially blocking call
	if cancel != nil {
		cancel()
	}
	if conn != nil {
		conn.Close()
	}
}

func (c *Consumer) worker(ctx context.Context) {
	defer c.workerWg.Done()
	for {
		select {
		case <-ctx.Done():
			return
		case j, ok := <-c.jobQueue:
			if !ok {
				return
			}

			var ev eventstream.Event
			err := json.Unmarshal(j.message, &ev)
			if err != nil {
				c.logger.Error("error deserializing message", "source", j.source.Key(), "err", err)
				continue
			}

			if err := c.cfg.ProcessFunc(ctx, j.source, ev); err != nil {
				c.logger.Error("error processing message", "source", j.source, "err", err)
			}

			c.advanceCursor(j.source, ev.Created)
		}
	}
}

func (c *Consumer) advanceCursor(s Source, newCursor int64) {
	if newCursor == 0 {
		return
	}
	c.sourcesMu.Lock()
	st, ok := c.sources[s]
	c.sourcesMu.Unlock()
	if !ok {
		return
	}

	st.cursorMu.Lock()
	defer st.cursorMu.Unlock()
	if newCursor <= st.cursorMax {
		return
	}
	st.cursorMax = newCursor
	c.cfg.CursorStore.Set(s.Key(), newCursor)
}

func (c *Consumer) startConnectionLoop(ctx context.Context, source Source) {
	defer c.sourceWg.Done()

	// attempt connection initially
	err := c.runConnection(ctx, source)
	if err != nil {
		c.logger.Error("failed to run connection", "err", err)
	}

	timer := time.NewTimer(1 * time.Minute)
	defer timer.Stop()

	// every subsequent attempt is delayed by 1 minute
	for {
		select {
		case <-ctx.Done():
			return
		case <-timer.C:
			err := c.runConnection(ctx, source)
			if err != nil {
				c.logger.Error("failed to run connection", "err", err)
			}
			timer.Reset(1 * time.Minute)
		}
	}
}

func (c *Consumer) runConnection(ctx context.Context, source Source) error {
	cursor := c.cfg.CursorStore.Get(source.Key())

	u, err := c.cfg.URLFunc(source, cursor)
	if err != nil {
		return err
	}

	c.logger.Info("connecting", "url", u.String())

	retryOpts := []retry.Option{
		retry.Attempts(c.cfg.MaxRetryAttempts),
		retry.DelayType(retry.BackOffDelay),
		retry.Delay(c.cfg.RetryInterval),
		retry.MaxDelay(c.cfg.MaxRetryInterval),
		retry.MaxJitter(c.cfg.RetryInterval / 5),
		retry.OnRetry(func(n uint, err error) {
			c.logger.Info("retrying connection",
				"source", source,
				"url", u.String(),
				"attempt", n+1,
				"err", err,
			)
		}),
		retry.Context(ctx),
	}

	var conn *websocket.Conn

	err = retry.Do(func() error {
		connCtx, cancel := context.WithTimeout(ctx, c.cfg.ConnectionTimeout)
		defer cancel()
		conn, _, err = c.dialer.DialContext(connCtx, u.String(), c.cfg.RequestHeader)
		return err
	}, retryOpts...)
	if err != nil {
		if c.cfg.OnConnectExceeded != nil {
			c.cfg.OnConnectExceeded(source, err)
		}
		return err
	}

	// Register the conn. If the source was removed (or our ctx cancelled)
	// while we were dialing, drop this conn instead of installing it.
	c.sourcesMu.Lock()
	st, ok := c.sources[source]
	if !ok || ctx.Err() != nil {
		c.sourcesMu.Unlock()
		conn.Close()
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return nil
	}
	st.conn = conn
	c.sourcesMu.Unlock()

	defer func() {
		// Clear the conn from state, but only if it's still our conn (a
		// concurrent RemoveSource may have already done it).
		c.sourcesMu.Lock()
		if st, ok := c.sources[source]; ok && st.conn == conn {
			st.conn = nil
		}
		c.sourcesMu.Unlock()
		conn.Close()
	}()

	c.logger.Info("connected", "source", source)

	for {
		select {
		case <-ctx.Done():
			return nil
		default:
			msgType, msg, err := conn.ReadMessage()
			if err != nil {
				return err
			}
			if msgType != websocket.TextMessage {
				continue
			}
			select {
			case c.jobQueue <- job{source: source, message: msg}:
			case <-ctx.Done():
				return nil
			}
		}
	}
}
