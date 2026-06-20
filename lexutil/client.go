package lexutil

import (
	"cmp"
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/carlmjohnson/versioninfo"
	"github.com/gorilla/websocket"
	cbg "github.com/whyrusleeping/cbor-gen"
)

const minHealthyConn = 30 * time.Second

type Client struct {
	indigoxrpc.Client
	Dialer websocket.Dialer
	Logger *slog.Logger
}

var _ LexClient = (*Client)(nil)

func makeParams(p map[string]any) url.Values {
	params := url.Values{}
	for k, v := range p {
		if s, ok := v.([]string); ok {
			for _, v := range s {
				params.Add(k, v)
			}
		} else {
			params.Add(k, fmt.Sprint(v))
		}
	}
	return params
}

type processFn func(ctx context.Context, cr *cbg.CborReader) error

func (c *Client) LexDo(ctx context.Context, method string, inputEncoding string, endpoint string, params map[string]any, bodyData any, out any) error {
	switch method {
	case Subscription:
		if process, ok := out.(func(context.Context, *cbg.CborReader) error); ok {
			return c.LexSubscribe(ctx, endpoint, params, process)
		} else if process, ok := out.(processFn); ok {
			return c.LexSubscribe(ctx, endpoint, params, process)
		} else if redialer, ok := out.(Redialer); ok {
			return c.LexSubscribeWithRedialer(ctx, endpoint, params, redialer)
		} else {
			return fmt.Errorf("unknown output type: %T", out)
		}
	default:
		return c.Client.LexDo(ctx, method, inputEncoding, endpoint, params, bodyData, out)
	}
}

func (c *Client) getHeader() http.Header {
	header := http.Header{}
	if c.UserAgent != nil {
		header.Set("User-Agent", *c.UserAgent)
	} else {
		header.Set("User-Agent", "extlexutil/"+versioninfo.Short())
	}
	if c.Headers != nil {
		for k, v := range c.Headers {
			header.Set(k, v)
		}
	}
	return header
}

func (c *Client) LexSubscribe(ctx context.Context, endpoint string, params map[string]any, process func(ctx context.Context, cr *cbg.CborReader) error) error {
	logger := cmp.Or(c.Logger, slog.Default().With("system", "events"))
	rurl, err := url.Parse(c.Host)
	if err != nil {
		return err
	}
	if rurl.Scheme == "http" {
		rurl.Scheme = "ws"
	} else {
		rurl.Scheme = "wss"
	}
	surl := rurl.JoinPath("/xrpc", endpoint)
	surl.RawQuery = makeParams(params).Encode()

	header := c.getHeader()

	u := surl.String()
	conn, resp, err := c.Dialer.DialContext(ctx, u, header)
	if err != nil {
		return fmt.Errorf("%w: %w", ErrDialFailure, err)
	}

	logger.Debug("event subscription response", "code", resp.StatusCode, "url", u)

	return c.handleConn(ctx, conn, process)
}

func (c *Client) LexSubscribeWithRedialer(ctx context.Context, endpoint string, params map[string]any, redialer Redialer) error {
	logger := cmp.Or(c.Logger, slog.Default().With("system", "events"))
	rurl, err := url.Parse(c.Host)
	if err != nil {
		return err
	}
	if rurl.Scheme == "http" {
		rurl.Scheme = "ws"
	} else {
		rurl.Scheme = "wss"
	}
	surl := rurl.JoinPath("/xrpc", endpoint)

	header := c.getHeader()

	var backoff int
	// returns false if the retry budget is exhausted
	sleepBackoff := func() bool {
		select {
		case <-ctx.Done():
		case <-time.After(time.Duration(5+backoff) * time.Second):
		}
		backoff++
		return backoff <= 15
	}

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		surl.RawQuery = makeParams(params).Encode()

		u := surl.String()
		conn, resp, err := c.Dialer.DialContext(ctx, u, header)
		if err != nil {
			logger.Warn("dialing failed", "err", err, "backoff", backoff)
			if !sleepBackoff() {
				return fmt.Errorf("%w: %w", ErrDialFailure, err)
			}
			continue
		}

		logger.Debug("event subscription response", "code", resp.StatusCode, "url", u)

		connectedAt := time.Now()
		connErr := c.handleConn(ctx, conn, redialer.Process)
		if connErr != nil {
			logger.Warn("host connection failed", "err", connErr, "backoff", backoff)
		}

		// updates cursor
		updated := redialer.UpdateParams(ctx, params)

		// a connection that drops immediately shouldnt reset backoff
		// this to avoid reconnect storms
		if updated || time.Since(connectedAt) >= minHealthyConn {
			backoff = 0
			continue
		}
		if !sleepBackoff() {
			return fmt.Errorf("%w: %w", ErrConnFailure, connErr)
		}
	}
}

func (c *Client) handleConn(ctx context.Context, conn *websocket.Conn, process func(ctx context.Context, cr *cbg.CborReader) error) error {
	logger := cmp.Or(c.Logger, slog.Default().With("system", "events"))
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	go func() {
		t := time.NewTicker(time.Second * 30)
		defer t.Stop()
		failcount := 0

		for {

			select {
			case <-t.C:
				if err := conn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(time.Second*10)); err != nil {
					logger.Warn("failed to ping", "err", err)
					failcount++
					if failcount >= 4 {
						logger.Error("too many ping fails", "count", failcount)
						conn.Close()
						return
					}
				} else {
					failcount = 0 // ok ping
				}
			case <-ctx.Done():
				conn.Close()
				return
			}
		}
	}()

	conn.SetPingHandler(func(message string) error {
		err := conn.WriteControl(websocket.PongMessage, []byte(message), time.Now().Add(time.Second*60))
		if err == websocket.ErrCloseSent {
			return nil
		}
		return err
	})

	conn.SetPongHandler(func(_ string) error {
		if err := conn.SetReadDeadline(time.Now().Add(time.Minute)); err != nil {
			logger.Error("failed to set read deadline", "err", err)
		}

		return nil
	})

	cr := new(cbg.CborReader)

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		mt, rawReader, err := conn.NextReader()
		if err != nil {
			return fmt.Errorf("conn err at read: %w", err)
		}

		if mt != websocket.BinaryMessage {
			return fmt.Errorf("expected binary message from subscription endpoint")
		}

		cr.SetReader(rawReader)

		if err := process(ctx, cr); err != nil {
			return err
		}
	}
}
