package knotmirror

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/netip"
	"net/url"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/knotstream"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/log"
	"tangled.org/core/tapc"
)

type Tap struct {
	logger *slog.Logger
	cfg    *config.Config
	tap    tapc.Client
	db     *sql.DB
	gitm   GitMirrorManager
	ks     *knotstream.KnotStream
}

func NewTapClient(l *slog.Logger, cfg *config.Config, db *sql.DB, gitm GitMirrorManager, ks *knotstream.KnotStream) *Tap {
	return &Tap{
		logger: log.SubLogger(l, "tapclient"),
		cfg:    cfg,
		tap:    tapc.NewClient(cfg.TapUrl, ""),
		db:     db,
		gitm:   gitm,
		ks:     ks,
	}
}

func (t *Tap) Start(ctx context.Context) {
	// TODO: better reconnect logic
	go func() {
		for {
			t.tap.Connect(ctx, &tapc.SimpleIndexer{
				EventHandler: t.processEvent,
			})
			time.Sleep(time.Second)
		}
	}()
}

func (t *Tap) processEvent(ctx context.Context, evt tapc.Event) error {
	l := t.logger.With("component", "tapIndexer")

	var err error
	switch evt.Type {
	case tapc.EvtRecord:
		switch evt.Record.Collection.String() {
		case tangled.RepoNSID:
			err = t.processRepo(ctx, evt.Record)
		}
	}

	if err != nil {
		l.Error("failed to process message. will retry later", "event.ID", evt.ID, "err", err)
		return err
	}
	return nil
}

func (t *Tap) processRepo(ctx context.Context, evt *tapc.RecordEventData) error {
	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.Repo{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			return fmt.Errorf("parsing record: %w", err)
		}

		knotUrl := record.Knot
		if !strings.Contains(record.Knot, "://") {
			if host, _ := db.GetHost(ctx, t.db, record.Knot); host != nil {
				knotUrl = host.URL()
			} else {
				t.logger.Warn("repo is from unknown knot")
				if t.cfg.KnotUseSSL {
					knotUrl = "https://" + knotUrl
				} else {
					knotUrl = "http://" + knotUrl
				}
			}
		}

		status := models.RepoStatePending
		errMsg := ""
		u, err := url.Parse(knotUrl)
		if err != nil {
			status = models.RepoStateSuspended
			errMsg = "failed to parse knot url"
		} else if t.cfg.KnotSSRF && isPrivate(u.Hostname()) {
			status = models.RepoStateSuspended
			errMsg = "suspending non-public knot"
		}

		if record.RepoDid == nil || *record.RepoDid == "" {
			t.logger.Warn("dropping repo record without repo_did", "did", evt.Did, "rkey", evt.Rkey)
			return nil
		}
		repoDid, err := syntax.ParseDID(*record.RepoDid)
		if err != nil {
			t.logger.Warn("dropping repo record with invalid DID", "did", evt.Did, "rkey", evt.Rkey, "repo", repoDid)
			return nil
		}
		repo := &models.Repo{
			Did:        evt.Did,
			Rkey:       evt.Rkey,
			Cid:        evt.CID,
			Name:       evt.Rkey.String(),
			KnotDomain: knotUrl,
			RepoDid:    repoDid,
			State:      status,
			ErrorMsg:   errMsg,
			RetryAfter: 0, // clear retry info
			RetryCount: 0,
		}

		if err := db.UpsertRepo(ctx, t.db, repo); err != nil {
			return fmt.Errorf("upserting repo to db: %w", err)
		}

		if !t.ks.CheckIfSubscribed(record.Knot) {
			if err := t.ks.SubscribeHost(ctx, record.Knot, !t.cfg.KnotUseSSL); err != nil {
				return fmt.Errorf("subscribing to knot: %w", err)
			}
		}

	case tapc.RecordDeleteAction:
		// no-op. deletion of sh.tangled.repo record doesn't mean repository deletion
	}
	return nil
}

// isPrivate checks if host is private network. It doesn't perform DNS resolution
func isPrivate(host string) bool {
	if host == "localhost" {
		return true
	}
	addr, err := netip.ParseAddr(host)
	if err != nil {
		return false
	}
	return isPrivateAddr(addr)
}

func isPrivateAddr(addr netip.Addr) bool {
	return addr.IsLoopback() ||
		addr.IsPrivate() ||
		addr.IsLinkLocalUnicast() ||
		addr.IsLinkLocalMulticast()
}
