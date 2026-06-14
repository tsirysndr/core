package lexutil

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/events"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/gorilla/websocket"
	cid "github.com/ipfs/go-cid"
	"github.com/stretchr/testify/assert"
	cbg "github.com/whyrusleeping/cbor-gen"
	xerrors "golang.org/x/xerrors"
)

type SubscribeExample_Foo struct {
	Seq int64
	Foo string
}

type SubscribeExample_Bar struct {
	Seq int64
	Bar string
}

type SubscribeExample_Event struct {
	Error *events.ErrorFrame
	Foo   *SubscribeExample_Foo
	Bar   *SubscribeExample_Bar
}

func (e *SubscribeExample_Event) seq() int64 {
	switch {
	case e.Foo != nil:
		return e.Foo.Seq
	case e.Bar != nil:
		return e.Bar.Seq
	default:
		return 0
	}
}

func (e *SubscribeExample_Event) Serialize(w io.Writer) error {
	cw := cbg.NewCborWriter(w)
	header := events.EventHeader{Op: events.EvtKindMessage}

	switch {
	case e.Error != nil:
		header.Op = events.EvtKindErrorFrame
		if err := header.MarshalCBOR(cw); err != nil {
			return err
		}
		return e.Error.MarshalCBOR(cw)
	case e.Foo != nil:
		header.MsgType = "#foo"
		if err := header.MarshalCBOR(cw); err != nil {
			return err
		}
		return e.Foo.MarshalCBOR(cw)
	case e.Bar != nil:
		header.MsgType = "#bar"
		if err := header.MarshalCBOR(cw); err != nil {
			return err
		}
		return e.Bar.MarshalCBOR(cw)
	default:
		return fmt.Errorf("unrecognized event kind")
	}
}

func (e *SubscribeExample_Event) Deserialize(r io.Reader) error {
	var header events.EventHeader
	if err := header.UnmarshalCBOR(r); err != nil {
		return fmt.Errorf("reading header: %w", err)
	}
	switch header.Op {
	case events.EvtKindMessage:
		switch header.MsgType {
		case "#foo":
			var evt SubscribeExample_Foo
			if err := evt.UnmarshalCBOR(r); err != nil {
				return err
			}
			e.Foo = &evt
		case "#bar":
			var evt SubscribeExample_Bar
			if err := evt.UnmarshalCBOR(r); err != nil {
				return err
			}
			e.Bar = &evt
		default:
			return fmt.Errorf("unknown message type: %s", header.MsgType)
		}
	case events.EvtKindErrorFrame:
		var errframe events.ErrorFrame
		if err := errframe.UnmarshalCBOR(r); err != nil {
			return err
		}
		e.Error = &errframe
	default:
		return fmt.Errorf("unrecognized event stream type: %d", header.Op)
	}
	return nil
}

func (t *SubscribeExample_Foo) MarshalCBOR(w io.Writer) error {
	if t == nil {
		_, err := w.Write(cbg.CborNull)
		return err
	}

	cw := cbg.NewCborWriter(w)

	if _, err := cw.Write([]byte{162}); err != nil {
		return err
	}

	// t.Foo (string) (string)
	if len("foo") > 1000000 {
		return xerrors.Errorf("Value in field \"foo\" was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len("foo"))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string("foo")); err != nil {
		return err
	}

	if len(t.Foo) > 1000000 {
		return xerrors.Errorf("Value in field t.Foo was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len(t.Foo))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string(t.Foo)); err != nil {
		return err
	}

	// t.Seq (int64) (int64)
	if len("seq") > 1000000 {
		return xerrors.Errorf("Value in field \"seq\" was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len("seq"))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string("seq")); err != nil {
		return err
	}

	if t.Seq >= 0 {
		if err := cw.WriteMajorTypeHeader(cbg.MajUnsignedInt, uint64(t.Seq)); err != nil {
			return err
		}
	} else {
		if err := cw.WriteMajorTypeHeader(cbg.MajNegativeInt, uint64(-t.Seq-1)); err != nil {
			return err
		}
	}
	return nil
}

func (t *SubscribeExample_Foo) UnmarshalCBOR(r io.Reader) (err error) {
	*t = SubscribeExample_Foo{}

	cr := cbg.NewCborReader(r)

	maj, extra, err := cr.ReadHeader()
	if err != nil {
		return err
	}
	defer func() {
		if err == io.EOF {
			err = io.ErrUnexpectedEOF
		}
	}()

	if maj != cbg.MajMap {
		return fmt.Errorf("cbor input should be of type map")
	}

	if extra > cbg.MaxLength {
		return fmt.Errorf("SubscribeExample_Foo: map struct too large (%d)", extra)
	}

	n := extra

	nameBuf := make([]byte, 8)
	for range n {
		nameLen, ok, err := cbg.ReadFullStringIntoBuf(cr, nameBuf, 1000000)
		if err != nil {
			return err
		}

		if !ok {
			// Field doesn't exist on this type, so ignore it
			if err := cbg.ScanForLinks(cr, func(cid.Cid) {}); err != nil {
				return err
			}
			continue
		}

		switch string(nameBuf[:nameLen]) {
		// t.Foo (string) (string)
		case "foo":

			{
				sval, err := cbg.ReadStringWithMax(cr, 1000000)
				if err != nil {
					return err
				}

				t.Foo = string(sval)
			}
			// t.Seq (int64) (int64)
		case "seq":
			{
				maj, extra, err := cr.ReadHeader()
				if err != nil {
					return err
				}
				var extraI int64
				switch maj {
				case cbg.MajUnsignedInt:
					extraI = int64(extra)
					if extraI < 0 {
						return fmt.Errorf("int64 positive overflow")
					}
				case cbg.MajNegativeInt:
					extraI = int64(extra)
					if extraI < 0 {
						return fmt.Errorf("int64 negative overflow")
					}
					extraI = -1 - extraI
				default:
					return fmt.Errorf("wrong type for int64 field: %d", maj)
				}

				t.Seq = int64(extraI)
			}

		default:
			// Field doesn't exist on this type, so ignore it
			if err := cbg.ScanForLinks(r, func(cid.Cid) {}); err != nil {
				return err
			}
		}
	}

	return nil
}

func (t *SubscribeExample_Bar) MarshalCBOR(w io.Writer) error {
	if t == nil {
		_, err := w.Write(cbg.CborNull)
		return err
	}

	cw := cbg.NewCborWriter(w)

	if _, err := cw.Write([]byte{162}); err != nil {
		return err
	}

	// t.Bar (string) (string)
	if len("bar") > 1000000 {
		return xerrors.Errorf("Value in field \"bar\" was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len("bar"))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string("bar")); err != nil {
		return err
	}

	if len(t.Bar) > 1000000 {
		return xerrors.Errorf("Value in field t.Bar was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len(t.Bar))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string(t.Bar)); err != nil {
		return err
	}

	// t.Seq (int64) (int64)
	if len("seq") > 1000000 {
		return xerrors.Errorf("Value in field \"seq\" was too long")
	}

	if err := cw.WriteMajorTypeHeader(cbg.MajTextString, uint64(len("seq"))); err != nil {
		return err
	}
	if _, err := cw.WriteString(string("seq")); err != nil {
		return err
	}

	if t.Seq >= 0 {
		if err := cw.WriteMajorTypeHeader(cbg.MajUnsignedInt, uint64(t.Seq)); err != nil {
			return err
		}
	} else {
		if err := cw.WriteMajorTypeHeader(cbg.MajNegativeInt, uint64(-t.Seq-1)); err != nil {
			return err
		}
	}
	return nil
}

func (t *SubscribeExample_Bar) UnmarshalCBOR(r io.Reader) (err error) {
	*t = SubscribeExample_Bar{}

	cr := cbg.NewCborReader(r)

	maj, extra, err := cr.ReadHeader()
	if err != nil {
		return err
	}
	defer func() {
		if err == io.EOF {
			err = io.ErrUnexpectedEOF
		}
	}()

	if maj != cbg.MajMap {
		return fmt.Errorf("cbor input should be of type map")
	}

	if extra > cbg.MaxLength {
		return fmt.Errorf("SubscribeExample_Bar: map struct too large (%d)", extra)
	}

	n := extra

	nameBuf := make([]byte, 8)
	for range n {
		nameLen, ok, err := cbg.ReadFullStringIntoBuf(cr, nameBuf, 1000000)
		if err != nil {
			return err
		}

		if !ok {
			// Field doesn't exist on this type, so ignore it
			if err := cbg.ScanForLinks(cr, func(cid.Cid) {}); err != nil {
				return err
			}
			continue
		}

		switch string(nameBuf[:nameLen]) {
		// t.Bar (string) (string)
		case "bar":

			{
				sval, err := cbg.ReadStringWithMax(cr, 1000000)
				if err != nil {
					return err
				}

				t.Bar = string(sval)
			}
			// t.Seq (int64) (int64)
		case "seq":
			{
				maj, extra, err := cr.ReadHeader()
				if err != nil {
					return err
				}
				var extraI int64
				switch maj {
				case cbg.MajUnsignedInt:
					extraI = int64(extra)
					if extraI < 0 {
						return fmt.Errorf("int64 positive overflow")
					}
				case cbg.MajNegativeInt:
					extraI = int64(extra)
					if extraI < 0 {
						return fmt.Errorf("int64 negative overflow")
					}
					extraI = -1 - extraI
				default:
					return fmt.Errorf("wrong type for int64 field: %d", maj)
				}

				t.Seq = int64(extraI)
			}

		default:
			// Field doesn't exist on this type, so ignore it
			if err := cbg.ScanForLinks(r, func(cid.Cid) {}); err != nil {
				return err
			}
		}
	}

	return nil
}

type testScheduler struct {
	ch      chan *SubscribeExample_Event
	lastSeq atomic.Int64
}

var _ SeqScheduler[SubscribeExample_Event] = (*testScheduler)(nil)

func newTestScheduler() *testScheduler {
	return &testScheduler{ch: make(chan *SubscribeExample_Event, 8)}
}

func (s *testScheduler) AddWork(ctx context.Context, namespace string, val *SubscribeExample_Event) error {
	s.lastSeq.Store(val.seq())
	select {
	case s.ch <- val:
	case <-ctx.Done():
	}
	return nil
}

func (s *testScheduler) Shutdown() {}

func (s *testScheduler) LastSeq() int64 { return s.lastSeq.Load() }

// recv returns the next scheduled event. The timeout is generous because a
// redial during the offline window backs off >=5s.
func (s *testScheduler) recv(t *testing.T) *SubscribeExample_Event {
	t.Helper()
	select {
	case e := <-s.ch:
		return e
	case <-time.After(12 * time.Second):
		t.Fatal("timed out waiting for an event")
		return nil
	}
}

type testRedialer struct {
	sched *testScheduler
}

var _ Redialer = (*testRedialer)(nil)

func (r *testRedialer) Process(ctx context.Context, cr *cbg.CborReader) error {
	var evt SubscribeExample_Event
	if err := evt.Deserialize(cr); err != nil {
		return err
	}
	return r.sched.AddWork(ctx, "", &evt)
}

func (r *testRedialer) UpdateParams(ctx context.Context, params map[string]any) bool {
	last := r.sched.LastSeq()
	if last == 0 {
		return false
	}
	params["cursor"] = last
	return true
}

const testEndpoint = "com.example.subscribeExample"

// testServer live-tails an append-only event log over a websocket: each
// connection streams events whose seq exceeds the requested cursor, including
// ones added after it opened. The httptest listener stays bound the whole time
// (so the port can't be taken over); Close/Start toggle offline by having the
// handler answer 404, which fails the client's websocket handshake and drives
// it into its redial/backoff loop.
type testServer struct {
	URL string

	mu      sync.Mutex
	events  []SubscribeExample_Event
	conns   map[*websocket.Conn]struct{}
	serving bool
}

func newTestServer(t *testing.T) *testServer {
	t.Helper()
	ts := &testServer{
		conns:   make(map[*websocket.Conn]struct{}),
		serving: true,
	}
	srv := httptest.NewServer(ts.handler())
	ts.URL = srv.URL
	t.Cleanup(func() {
		ts.Close() // drop hijacked conns before srv.Close so it won't block
		srv.Close()
	})
	return ts
}

func (ts *testServer) handler() http.Handler {
	up := websocket.Upgrader{}
	mux := http.NewServeMux()
	mux.HandleFunc("/xrpc/"+testEndpoint, func(w http.ResponseWriter, r *http.Request) {
		if !ts.isServing() {
			http.Error(w, "offline", http.StatusNotFound)
			return
		}

		var cursor int64
		if c := r.URL.Query().Get("cursor"); c != "" {
			cursor, _ = strconv.ParseInt(c, 10, 64)
		}

		conn, err := up.Upgrade(w, r, nil)
		if err != nil {
			return
		}

		ts.mu.Lock()
		ts.conns[conn] = struct{}{}
		ts.mu.Unlock()
		defer func() {
			ts.mu.Lock()
			delete(ts.conns, conn)
			ts.mu.Unlock()
			conn.Close()
		}()

		sent := cursor
		for {
			ts.mu.Lock()
			var batch []SubscribeExample_Event
			for _, e := range ts.events {
				if e.seq() > sent {
					batch = append(batch, e)
				}
			}
			ts.mu.Unlock()

			for i := range batch {
				if err := func(conn *websocket.Conn, e *SubscribeExample_Event) error {
					var buf bytes.Buffer
					if err := e.Serialize(&buf); err != nil {
						return err
					}
					return conn.WriteMessage(websocket.BinaryMessage, buf.Bytes())
				}(conn, &batch[i]); err != nil {
					return
				}
				sent = batch[i].seq()
			}
			time.Sleep(5 * time.Millisecond)
		}
	})
	return mux
}

func (ts *testServer) isServing() bool {
	ts.mu.Lock()
	defer ts.mu.Unlock()
	return ts.serving
}

// Start brings the server back online.
func (ts *testServer) Start() {
	ts.mu.Lock()
	ts.serving = true
	ts.mu.Unlock()
}

// Close takes the server offline: new handshakes get 404 and active websocket
// connections are dropped, forcing the subscription to redial.
func (ts *testServer) Close() {
	ts.mu.Lock()
	ts.serving = false
	conns := make([]*websocket.Conn, 0, len(ts.conns))
	for c := range ts.conns {
		conns = append(conns, c)
	}
	ts.mu.Unlock()

	for _, c := range conns {
		c.Close()
	}
}

func (ts *testServer) AddEvent(e SubscribeExample_Event) {
	ts.mu.Lock()
	ts.events = append(ts.events, e)
	ts.mu.Unlock()
}

func waitForReturn(t *testing.T, done <-chan error) {
	t.Helper()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("subscription did not return after context cancel")
	}
}

func TestLexSubscribe_ConsumesAndSchedules(t *testing.T) {
	srv := newTestServer(t)
	defer srv.Close()

	c := &Client{Client: indigoxrpc.Client{Host: srv.URL}}
	sched := newTestScheduler()

	process := func(ctx context.Context, cr *cbg.CborReader) error {
		var evt SubscribeExample_Event
		if err := evt.Deserialize(cr); err != nil {
			return err
		}
		return sched.AddWork(ctx, "", &evt)
	}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- c.LexSubscribe(ctx, testEndpoint, map[string]any{"cursor": int64(0)}, process)
	}()

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}})
	srv.AddEvent(SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}})
	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}}, sched.recv(t))
	assert.Equal(t, &SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}}, sched.recv(t))

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}})
	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}}, sched.recv(t))

	cancel()
	waitForReturn(t, done)
}

func TestLexSubscribeWithRedialer_ConsumesAndSchedules(t *testing.T) {
	srv := newTestServer(t)
	defer srv.Close()

	c := &Client{Client: indigoxrpc.Client{Host: srv.URL}}
	sched := newTestScheduler()
	redialer := &testRedialer{sched: sched}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- c.LexSubscribeWithRedialer(ctx, testEndpoint, map[string]any{"cursor": int64(0)}, redialer)
	}()

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}})
	srv.AddEvent(SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}})
	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}}, sched.recv(t))
	assert.Equal(t, &SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}}, sched.recv(t))

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}})
	srv.AddEvent(SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 4, Bar: "bar-4"}})
	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}}, sched.recv(t))
	assert.Equal(t, &SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 4, Bar: "bar-4"}}, sched.recv(t))

	cancel()
	waitForReturn(t, done)
}

func TestLexSubscribeWithRedialer_HandlesDowntime(t *testing.T) {
	srv := newTestServer(t)
	defer srv.Close()

	c := &Client{Client: indigoxrpc.Client{Host: srv.URL}}
	sched := newTestScheduler()
	redialer := &testRedialer{sched: sched}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- c.LexSubscribeWithRedialer(ctx, testEndpoint, map[string]any{"cursor": int64(0)}, redialer)
	}()

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}})
	srv.AddEvent(SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}})
	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 1, Foo: "foo-1"}}, sched.recv(t))
	assert.Equal(t, &SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 2, Bar: "bar-2"}}, sched.recv(t))

	// offline, add events, back online: the subscription redials and resumes
	srv.Close()

	srv.AddEvent(SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}})
	srv.AddEvent(SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 4, Bar: "bar-4"}})

	srv.Start()

	assert.Equal(t, &SubscribeExample_Event{Foo: &SubscribeExample_Foo{Seq: 3, Foo: "foo-3"}}, sched.recv(t))
	assert.Equal(t, &SubscribeExample_Event{Bar: &SubscribeExample_Bar{Seq: 4, Bar: "bar-4"}}, sched.recv(t))

	cancel()
	waitForReturn(t, done)
}
