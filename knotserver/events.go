package knotserver

import (
	"context"
	"errors"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventstream"
	"tangled.org/core/log"
)

func (h *Knot) Events(w http.ResponseWriter, r *http.Request) {
	l := log.SubLogger(h.l, "eventstream")
	l.Debug("received new connection")

	err := eventstream.Stream(w, r, eventstream.StreamConfig{
		Backend:  h.db,
		Notifier: h.n,
		Logger:   l,
	})
	if err != nil && !errors.Is(err, eventstream.ErrDrainCap) {
		l.Error("event stream ended with error", "err", err)
	}

	go func() {
		retryCtx, retryCancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer retryCancel()
		if err := h.requestCrawl(retryCtx); err != nil {
			l.Error("error requesting crawls", "err", err)
		}
	}()
}

func (h *Knot) requestCrawl(ctx context.Context) error {
	h.l.Info("requesting crawl", "mirrors", h.c.KnotMirrors)
	input := &tangled.SyncRequestCrawl_Input{
		Hostname: h.c.Server.Hostname,
	}
	for _, knotmirror := range h.c.KnotMirrors {
		xrpcc := xrpc.Client{Host: knotmirror}
		if err := tangled.SyncRequestCrawl(ctx, &xrpcc, input); err != nil {
			h.l.Error("error requesting crawl", "err", err)
		} else {
			h.l.Info("crawl requested successfully")
		}
	}
	return nil
}
