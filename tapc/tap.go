/// heavily inspired by <https://github.com/bluesky-social/atproto/blob/c7f5a868837d3e9b3289f988fee2267789327b06/packages/tap/README.md>

package tapc

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/gorilla/websocket"
	"tangled.org/core/log"
)

type Handler interface {
	OnEvent(ctx context.Context, evt Event) error
	OnError(ctx context.Context, err error)
}

type Client struct {
	Url           string
	AdminPassword string
	HTTPClient    *http.Client
}

func NewClient(url, adminPassword string) Client {
	return Client{
		Url:           url,
		AdminPassword: adminPassword,
		HTTPClient:    &http.Client{},
	}
}

func (c *Client) AddRepos(ctx context.Context, dids []syntax.DID) error {
	body, err := json.Marshal(map[string][]syntax.DID{"dids": dids})
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, "POST", c.Url+"/repos/add", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.SetBasicAuth("admin", c.AdminPassword)
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.HTTPClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("tap: /repos/add failed with status %d", resp.StatusCode)
	}
	return nil
}

func (c *Client) RemoveRepos(ctx context.Context, dids []syntax.DID) error {
	body, err := json.Marshal(map[string][]syntax.DID{"dids": dids})
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, "POST", c.Url+"/repos/remove", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.SetBasicAuth("admin", c.AdminPassword)
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.HTTPClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("tap: /repos/remove failed with status %d", resp.StatusCode)
	}
	return nil
}

func (c *Client) Connect(ctx context.Context, handler Handler) error {
	l := log.FromContext(ctx)

	u, err := url.Parse(c.Url)
	if err != nil {
		return err
	}
	if u.Scheme == "https" {
		u.Scheme = "wss"
	} else {
		u.Scheme = "ws"
	}
	u.Path = "/channel"

	// TODO: set auth on dial

	url := u.String()

	var backoff int
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		header := http.Header{
			"Authorization": []string{""},
		}
		conn, res, err := websocket.DefaultDialer.DialContext(ctx, url, header)
		if err != nil {
			l.Warn("dialing failed", "url", url, "err", err, "backoff", backoff)
			time.Sleep(time.Duration(5+backoff) * time.Second)
			backoff++

			continue
		}
		l.Info("connected to tap service")

		l.Info("tap event subscription response", "code", res.StatusCode)

		if err = c.handleConnection(ctx, conn, handler); err != nil {
			l.Warn("tap connection failed", "err", err, "backoff", backoff)
		}
	}
}

func (c *Client) handleConnection(ctx context.Context, conn *websocket.Conn, handler Handler) error {
	l := log.FromContext(ctx)

	defer func() {
		conn.Close()
		l.Warn("closed tap conection")
	}()
	l.Info("established tap conection")

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		_, message, err := conn.ReadMessage()
		if err != nil {
			return err
		}

		var ev Event
		if err := json.Unmarshal(message, &ev); err != nil {
			handler.OnError(ctx, fmt.Errorf("failed to parse message: %w", err))
			continue
		}
		if err := handler.OnEvent(ctx, ev); err != nil {
			handler.OnError(ctx, fmt.Errorf("failed to process event %d: %w", ev.ID, err))
			continue
		}

		ack := map[string]any{
			"type": "ack",
			"id":   ev.ID,
		}
		if err := conn.WriteJSON(ack); err != nil {
			l.Warn("failed to send ack", "err", err)
			continue
		}
	}
}
