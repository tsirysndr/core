package knotstream

import (
	"context"
	"database/sql"
	"fmt"
	"log/slog"
	"time"

	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/log"
)

type KnotStream struct {
	logger  *slog.Logger
	db      *sql.DB
	slurper *KnotSlurper
}

func NewKnotStream(l *slog.Logger, db *sql.DB, cfg *config.Config) *KnotStream {
	l = log.SubLogger(l, "knotstream")
	return &KnotStream{
		logger:  l,
		db:      db,
		slurper: NewKnotSlurper(l, db, cfg),
	}
}

func (s *KnotStream) Start(ctx context.Context) {
	go s.slurper.Run(ctx)
}

func (s *KnotStream) Shutdown(ctx context.Context) error {
	return s.slurper.Shutdown(ctx)
}

func (s *KnotStream) CheckIfSubscribed(hostname string) bool {
	return s.slurper.CheckIfSubscribed(hostname)
}

func (s *KnotStream) SubscribeHost(ctx context.Context, hostname string, noSSL bool) error {
	l := s.logger.With("hostname", hostname, "nossl", noSSL)
	l.Debug("subscribe")
	host, err := db.GetHost(ctx, s.db, hostname)
	if err != nil {
		return fmt.Errorf("loading host from db: %w", err)
	}

	if host == nil {
		host = &models.Host{
			Hostname: hostname,
			NoSSL:    noSSL,
			Status:   models.HostStatusActive,
			LastSeq:  0,
		}

		if err := db.UpsertHost(ctx, s.db, host); err != nil {
			return fmt.Errorf("adding host to db: %w", err)
		}

		l.Info("adding new host subscription")
	}

	if host.Status == models.HostStatusBanned {
		return fmt.Errorf("cannot subscribe to banned knot")
	}
	// `Subscribe` expects long-living context
	if err := s.slurper.Subscribe(*host); err != nil {
		return fmt.Errorf("slurper: %w", err)
	}

	host.Status = models.HostStatusActive
	if err := db.UpsertHost(ctx, s.db, host); err != nil {
		return fmt.Errorf("upserting host status to db: %w", err)
	}

	return nil
}

func (s *KnotStream) ResubscribeAllHosts(ctx context.Context) error {
	hosts, err := db.ListHosts(ctx, s.db, models.HostStatusActive)
	if err != nil {
		return fmt.Errorf("listing hosts: %w", err)
	}

	for _, host := range hosts {
		l := s.logger.With("hostname", host.Hostname)
		l.Info("re-subscribing to active host")
		if err := s.slurper.Subscribe(host); err != nil {
			l.Warn("failed to re-subscribe to host", "err", err)
		}
		// sleep for a very short period, so we don't open tons of sockets at the same time
		time.Sleep(1 * time.Millisecond)
	}
	return nil
}
