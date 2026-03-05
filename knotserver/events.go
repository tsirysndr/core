package knotserver

import (
	"context"
	"encoding/json"
	"net/http"
	"strconv"
	"time"

	"github.com/bluesky-social/indigo/xrpc"
	"github.com/gorilla/websocket"
	"tangled.org/core/api/tangled"
	"tangled.org/core/log"
)

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

func (h *Knot) Events(w http.ResponseWriter, r *http.Request) {
	l := log.SubLogger(h.l, "eventstream")
	l.Debug("received new connection")

	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		l.Error("websocket upgrade failed", "err", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
	defer conn.Close()
	l.Debug("upgraded http to wss")

	ch := h.n.Subscribe()
	defer h.n.Unsubscribe(ch)

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()
	go func() {
		for {
			if _, _, err := conn.NextReader(); err != nil {
				l.Error("failed to read", "err", err)
				cancel()
				return
			}
		}
	}()

	defaultCursor := time.Now().UnixNano()
	cursorStr := r.URL.Query().Get("cursor")
	cursor, err := strconv.ParseInt(cursorStr, 10, 64)
	if err != nil {
		l.Error("empty or invalid cursor", "invalidCursor", cursorStr, "default", defaultCursor)
	}
	if cursor == 0 {
		cursor = defaultCursor
	}

	l.Debug("going through backfill", "cursor", cursor)
	if err := h.drainBackfill(conn, &cursor, 10_000); err != nil {
		l.Error("failed to backfill", "err", err)
		return
	}

	// try request crawl when connection closed
	defer func() {
		go func() {
			retryCtx, retryCancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer retryCancel()
			if err := h.requestCrawl(retryCtx); err != nil {
				l.Error("error requesting crawls", "err", err)
			}
		}()
	}()

	for {
		// wait for new data or timeout
		select {
		case <-ctx.Done():
			l.Debug("stopping stream: client closed connection")
			return
		case <-ch:
			l.Debug("going through live data", "cursor", cursor)
			if _, err := h.streamOps(conn, &cursor); err != nil {
				l.Error("failed to stream", "err", err)
				return
			}
		case <-time.After(30 * time.Second):
			// send a keep-alive
			if err = conn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(time.Second)); err != nil {
				l.Error("failed to write control", "err", err)
			}
		}
	}
}

func (h *Knot) drainBackfill(conn *websocket.Conn, cursor *int64, maxBatches int) error {
	for range maxBatches {
		n, err := h.streamOps(conn, cursor)
		if err != nil {
			return err
		}
		if n < 100 {
			return nil
		}
	}
	h.l.Warn("backfill hit batch limit", "maxBatches", maxBatches, "cursor", *cursor)
	return nil
}

func (h *Knot) streamOps(conn *websocket.Conn, cursor *int64) (int, error) {
	events, err := h.db.GetEvents(*cursor)
	if err != nil {
		h.l.Error("failed to fetch events from db", "err", err, "cursor", cursor)
		return 0, err
	}

	for _, event := range events {
		var eventJson map[string]any
		err := json.Unmarshal([]byte(event.EventJson), &eventJson)
		if err != nil {
			h.l.Error("failed to unmarshal event", "err", err)
			return 0, err
		}

		jsonMsg, err := json.Marshal(map[string]any{
			"rkey":    event.Rkey,
			"nsid":    event.Nsid,
			"event":   eventJson,
			"created": event.Created,
		})
		if err != nil {
			h.l.Error("failed to marshal record", "err", err)
			return 0, err
		}

		if err := conn.WriteMessage(websocket.TextMessage, jsonMsg); err != nil {
			h.l.Debug("err", "err", err)
			return 0, err
		}
		*cursor = event.Created
	}

	return len(events), nil
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
