package eventstream

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	_ "github.com/mattn/go-sqlite3"
	"tangled.org/core/notifier"
)

type memSource struct {
	mu     sync.Mutex
	events []Event
}

func (s *memSource) add(ev Event) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.events = append(s.events, ev)
}

func (s *memSource) GetEvents(cursor int64, limit int) ([]Event, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := []Event{}
	for _, ev := range s.events {
		if ev.Created > cursor {
			out = append(out, ev)
			if len(out) == limit {
				break
			}
		}
	}
	return out, nil
}

func mkEvent(i int) Event {
	return Event{
		Rkey:      fmt.Sprintf("rk-%04d", i),
		Nsid:      "sh.tangled.test",
		EventJson: json.RawMessage(fmt.Sprintf(`{"i":%d}`, i)),
		Created:   int64(i + 1),
	}
}

func startServer(t *testing.T, src Backend, cfg StreamConfig) (string, *notifier.Notifier, <-chan error) {
	t.Helper()
	n := notifier.New()
	cfg.Backend = src
	cfg.Notifier = &n
	cfg.Logger = slog.New(slog.NewTextHandler(io.Discard, nil))

	errCh := make(chan error, 1)
	mux := http.NewServeMux()
	mux.HandleFunc("/events", func(w http.ResponseWriter, r *http.Request) {
		errCh <- Stream(w, r, cfg)
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http") + "/events"
	return wsURL, &n, errCh
}

func dial(t *testing.T, wsURL string, cursor int64) *websocket.Conn {
	t.Helper()
	if cursor != 0 {
		wsURL += "?cursor=" + strconv.FormatInt(cursor, 10)
	}
	c, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

func readN(t *testing.T, c *websocket.Conn, n int) []Event {
	t.Helper()
	c.SetReadDeadline(time.Now().Add(2 * time.Second))
	out := make([]Event, 0, n)
	for range n {
		_, msg, err := c.ReadMessage()
		if err != nil {
			t.Fatalf("read message at %d/%d: %v", len(out), n, err)
		}
		var ev Event
		if err := json.Unmarshal(msg, &ev); err != nil {
			t.Fatalf("unmarshal: %v", err)
		}
		out = append(out, ev)
	}
	return out
}

func TestStream_DrainStopsOnShortBatch(t *testing.T) {
	src := &memSource{}
	for i := range 7 {
		src.add(mkEvent(i))
	}

	wsURL, _, errCh := startServer(t, src, StreamConfig{
		BatchSize:          3,
		MaxBatchesPerDrain: 10,
	})
	c := dial(t, wsURL, 0)

	got := readN(t, c, 7)
	for i, ev := range got {
		if ev.Created != int64(i+1) {
			t.Fatalf("event %d: got created=%d", i, ev.Created)
		}
	}

	c.Close()
	select {
	case err := <-errCh:
		if err != nil && !isCloseErr(err) {
			t.Fatalf("server error: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("server did not exit")
	}
}

func TestStream_DrainHitsCap_ReturnsErrDrainCap(t *testing.T) {
	src := &memSource{}
	for i := range 5 {
		src.add(mkEvent(i))
	}

	wsURL, _, errCh := startServer(t, src, StreamConfig{
		BatchSize:          2,
		MaxBatchesPerDrain: 2,
	})
	c := dial(t, wsURL, 0)

	got := readN(t, c, 4)
	if len(got) != 4 {
		t.Fatalf("want 4 events before cap, got %d", len(got))
	}
	if got[3].Created != 4 {
		t.Fatalf("last delivered created = %d, want 4", got[3].Created)
	}

	select {
	case err := <-errCh:
		if !errors.Is(err, ErrDrainCap) {
			t.Fatalf("want ErrDrainCap, got %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("server did not return cap error")
	}
}

func TestStream_CursorResume(t *testing.T) {
	src := &memSource{}
	for i := range 5 {
		src.add(mkEvent(i))
	}

	wsURL, _, errCh := startServer(t, src, StreamConfig{
		BatchSize:          10,
		MaxBatchesPerDrain: 10,
	})
	c := dial(t, wsURL, 3)

	got := readN(t, c, 2)
	if got[0].Created != 4 || got[1].Created != 5 {
		t.Fatalf("resume from cursor: got %d,%d want 4,5", got[0].Created, got[1].Created)
	}

	c.Close()
	<-errCh
}

func TestStream_LiveDelivery(t *testing.T) {
	src := &memSource{}

	wsURL, n, errCh := startServer(t, src, StreamConfig{
		BatchSize:          10,
		MaxBatchesPerDrain: 10,
	})
	c := dial(t, wsURL, 0)

	src.add(mkEvent(42))
	n.NotifyAll()

	got := readN(t, c, 1)
	if got[0].Created != 43 {
		t.Fatalf("live event created = %d, want 43", got[0].Created)
	}

	c.Close()
	<-errCh
}

func TestStream_LiveBurstExceedsBatchSize_DrainsAll(t *testing.T) {
	src := &memSource{}

	wsURL, n, errCh := startServer(t, src, StreamConfig{
		BatchSize:          5,
		MaxBatchesPerDrain: 100,
	})
	c := dial(t, wsURL, 0)

	const burst = 17
	for i := range burst {
		src.add(mkEvent(i))
	}
	n.NotifyAll()

	got := readN(t, c, burst)
	if len(got) != burst {
		t.Fatalf("got %d events, want %d", len(got), burst)
	}
	for i, ev := range got {
		if ev.Created != int64(i+1) {
			t.Fatalf("event %d: got created=%d want %d", i, ev.Created, i+1)
		}
	}

	c.Close()
	<-errCh
}

func TestInsert_MonotonicCreatedUnderConcurrency(t *testing.T) {
	db, err := sql.Open("sqlite3", t.TempDir()+"/events.db")
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { db.Close() })
	if _, err := db.Exec(`create table events (
		rkey text not null,
		nsid text not null,
		event text not null,
		created integer not null,
		primary key (rkey, nsid)
	)`); err != nil {
		t.Fatalf("schema: %v", err)
	}

	n := notifier.New()
	const total = 300
	var wg sync.WaitGroup
	for i := range total {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			if err := Insert(db, Event{
				Rkey:      fmt.Sprintf("rk-%d", i),
				Nsid:      "sh.tangled.test",
				EventJson: json.RawMessage("{}"),
			}, &n); err != nil {
				t.Errorf("insert %d: %v", i, err)
			}
		}(i)
	}
	wg.Wait()

	rows, err := db.Query(`select created from events order by created asc`)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	defer rows.Close()

	var prev int64
	count := 0
	for rows.Next() {
		var c int64
		if err := rows.Scan(&c); err != nil {
			t.Fatalf("scan: %v", err)
		}
		if count > 0 && c <= prev {
			t.Fatalf("created not strictly increasing: %d <= %d", c, prev)
		}
		prev = c
		count++
	}
	if count != total {
		t.Fatalf("got %d rows, want %d", count, total)
	}
}

func TestHighWaterSeedsClockFromStoredEvents(t *testing.T) {
	db, err := sql.Open("sqlite3", t.TempDir()+"/events.db")
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { db.Close() })
	if _, err := db.Exec(`create table events (
		rkey text not null,
		nsid text not null,
		event text not null,
		created integer not null,
		primary key (rkey, nsid)
	)`); err != nil {
		t.Fatalf("schema: %v", err)
	}

	stored := time.Now().Add(time.Hour).UnixNano()
	if _, err := db.Exec(
		`insert into events (rkey, nsid, event, created) values (?, ?, ?, ?)`,
		"stored", "sh.tangled.test", "{}", stored,
	); err != nil {
		t.Fatalf("seed event: %v", err)
	}

	cut, err := HighWater(db)
	if err != nil {
		t.Fatalf("HighWater() error = %v", err)
	}
	if cut < stored {
		t.Fatalf("HighWater() = %d, want at least stored cursor %d", cut, stored)
	}

	n := notifier.New()
	if err := Insert(db, Event{
		Rkey:      "new",
		Nsid:      "sh.tangled.test",
		EventJson: json.RawMessage("{}"),
	}, &n); err != nil {
		t.Fatalf("Insert() error = %v", err)
	}
	events, err := List(db, cut, 10)
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(events) != 1 || events[0].Rkey != "new" || events[0].Created <= cut {
		t.Fatalf("events after cut = %+v, want only new event above %d", events, cut)
	}
}

func isCloseErr(err error) bool {
	if err == nil {
		return false
	}
	if websocket.IsCloseError(err, websocket.CloseNormalClosure, websocket.CloseGoingAway, websocket.CloseAbnormalClosure) {
		return true
	}
	return strings.Contains(err.Error(), "use of closed network connection") ||
		strings.Contains(err.Error(), "websocket: close") ||
		strings.Contains(err.Error(), "broken pipe")
}
