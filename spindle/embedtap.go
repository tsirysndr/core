package spindle

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"strings"
	"sync/atomic"
	"time"

	"github.com/bluesky-social/indigo/service/tap"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
)

func randomAdminPassword() (string, error) {
	var b [32]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", fmt.Errorf("generate tap admin password: %w", err)
	}
	return hex.EncodeToString(b[:]), nil
}

func assertLoopbackBind(bind string) error {
	host, _, err := net.SplitHostPort(bind)
	if err != nil {
		return fmt.Errorf("parse tap bind %q: %w", bind, err)
	}
	if host == "" {
		return fmt.Errorf("embedded mode requires loopback host in tap bind %q", bind)
	}
	if strings.EqualFold(host, "localhost") {
		return nil
	}
	ip := net.ParseIP(host)
	if ip == nil || !ip.IsLoopback() {
		return fmt.Errorf("embedded tap bind %q must be loopback like 127.0.0.1 or ::1", bind)
	}
	return nil
}

type embeddedTap struct {
	tap    *tap.Tap
	logger *slog.Logger
	closed atomic.Bool
}

func startEmbeddedTap(ctx context.Context, cfg *config.Config, logger *slog.Logger) (*embeddedTap, error) {
	if err := assertLoopbackBind(cfg.Server.Tap.Bind); err != nil {
		return nil, err
	}

	tcfg := tap.Config{
		DatabaseURL:                "sqlite://" + cfg.Server.Tap.DBPath,
		DBMaxConns:                 32,
		PLCURL:                     cfg.Server.PlcUrl,
		RelayUrl:                   cfg.Server.Tap.RelayUrl,
		FirehoseParallelism:        4,
		ResyncParallelism:          2,
		OutboxParallelism:          1,
		FirehoseCursorSaveInterval: time.Second,
		RepoFetchTimeout:           5 * time.Minute,
		IdentityCacheSize:          50_000,
		EventCacheSize:             10_000,
		SignalCollection:           tangled.RepoNSID,
		CollectionFilters:          []string{tangled.RepoNSID, tangled.RepoCollaboratorNSID},
		AdminPassword:              cfg.Server.Tap.AdminPassword,
		RetryTimeout:               60 * time.Second,
	}

	t, err := tap.New(tcfg)
	if err != nil {
		return nil, fmt.Errorf("tap.New: %w", err)
	}

	go func() {
		if err := t.Firehose.Run(ctx); err != nil && !errors.Is(err, context.Canceled) {
			logger.Error("firehose terminated", "err", err)
		}
	}()
	t.Run(ctx)
	go func() {
		logger.Info("tap http server listening", "bind", cfg.Server.Tap.Bind)
		if err := t.Server.Start(cfg.Server.Tap.Bind); err != nil && !errors.Is(err, http.ErrServerClosed) {
			logger.Error("tap http server terminated", "err", err)
		}
	}()

	if err := waitForListener(ctx, cfg.Server.Tap.Bind, time.Now().Add(10*time.Second)); err != nil {
		logger.Warn("tap http server unreachable before timeout", "bind", cfg.Server.Tap.Bind, "err", err)
	}

	return &embeddedTap{tap: t, logger: logger}, nil
}

func waitForListener(ctx context.Context, addr string, deadline time.Time) error {
	if ctx.Err() != nil {
		return ctx.Err()
	}
	if time.Now().After(deadline) {
		return fmt.Errorf("timed out waiting for %s", addr)
	}
	c, err := net.DialTimeout("tcp", addr, 100*time.Millisecond)
	if err == nil {
		c.Close()
		return nil
	}
	time.Sleep(50 * time.Millisecond)
	return waitForListener(ctx, addr, deadline)
}

func (e *embeddedTap) Shutdown() {
	if e == nil || e.tap == nil {
		return
	}
	if e.closed.Swap(true) {
		return
	}
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := e.tap.Server.Shutdown(shutdownCtx); err != nil {
		e.logger.Error("tap server shutdown failed", "err", err)
	}
	if err := e.tap.CloseDb(shutdownCtx); err != nil {
		e.logger.Error("tap db close failed", "err", err)
	}
}
