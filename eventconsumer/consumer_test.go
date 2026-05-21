package eventconsumer

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
	"tangled.org/core/notifier"
)

type memSrc struct {
	mu     sync.Mutex
	events []eventstream.Event
}

func (s *memSrc) add(ev eventstream.Event) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.events = append(s.events, ev)
}

func (s *memSrc) GetEvents(cursor int64, limit int) ([]eventstream.Event, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := []eventstream.Event{}
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

func mkEv(i int) eventstream.Event {
	return eventstream.Event{
		Rkey:      fmt.Sprintf("rk-%04d", i),
		Nsid:      "sh.tangled.test",
		EventJson: json.RawMessage(fmt.Sprintf(`{"i":%d}`, i)),
		Created:   int64(i + 1),
	}
}

func startEventServer(t *testing.T, src *memSrc) (Source, *notifier.Notifier) {
	t.Helper()
	n := notifier.New()
	mux := http.NewServeMux()
	mux.HandleFunc("/events", func(w http.ResponseWriter, r *http.Request) {
		_ = eventstream.Stream(w, r, eventstream.StreamConfig{
			Backend:            src,
			Notifier:           &n,
			Logger:             slog.New(slog.NewTextHandler(io.Discard, nil)),
			BatchSize:          5,
			MaxBatchesPerDrain: 100,
		})
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	addr := strings.TrimPrefix(srv.URL, "http://")
	return Source{Kind: "test", Host: addr}, &n
}

func TestConsumer_DrainAdvancesCursor(t *testing.T) {
	src := &memSrc{}
	for i := range 8 {
		src.add(mkEv(i))
	}

	source, _ := startEventServer(t, src)

	store := &cursor.MemoryStore{}
	seenMu := sync.Mutex{}
	seen := []int64{}

	cfg := ConsumerConfig{
		ProcessFunc: func(ctx context.Context, _ Source, msg eventstream.Event) error {
			seenMu.Lock()
			seen = append(seen, msg.Created)
			seenMu.Unlock()
			return nil
		},
		WorkerCount:       1,
		QueueSize:         16,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       store,
		URLFunc:           DefaultURL(true),
		Logger:            slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	c := NewConsumer(cfg)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	c.Start(ctx)
	c.AddSource(ctx, source)

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		seenMu.Lock()
		n := len(seen)
		seenMu.Unlock()
		if n >= 8 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	seenMu.Lock()
	defer seenMu.Unlock()
	if len(seen) != 8 {
		t.Fatalf("processed %d events, want 8: %v", len(seen), seen)
	}
	for i, got := range seen {
		if got != int64(i+1) {
			t.Fatalf("event %d: got created=%d want %d", i, got, i+1)
		}
	}

	if final := store.Get(source.Key()); final != 8 {
		t.Fatalf("cursor = %d, want 8", final)
	}
}

func TestConsumer_CursorMonotonic_OutOfOrderWorkers(t *testing.T) {
	src := &memSrc{}
	for i := range 4 {
		src.add(mkEv(i))
	}

	source, _ := startEventServer(t, src)

	store := &cursor.MemoryStore{}

	releaseFirst := make(chan struct{})
	processed := make(chan int64, 4)

	cfg := ConsumerConfig{
		ProcessFunc: func(ctx context.Context, _ Source, msg eventstream.Event) error {
			if msg.Created == 1 {
				<-releaseFirst
			}
			processed <- msg.Created
			return nil
		},
		WorkerCount:       4,
		QueueSize:         16,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       store,
		URLFunc:           DefaultURL(true),
		Logger:            slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	c := NewConsumer(cfg)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	c.Start(ctx)
	c.AddSource(ctx, source)

	for range 3 {
		select {
		case <-processed:
		case <-time.After(3 * time.Second):
			t.Fatal("timed out waiting for events 2-4 to be processed")
		}
	}

	if cur := store.Get(source.Key()); cur != 4 {
		t.Fatalf("cursor before slow worker finished = %d, want 4", cur)
	}

	close(releaseFirst)
	select {
	case <-processed:
	case <-time.After(3 * time.Second):
		t.Fatal("timed out waiting for slow worker")
	}

	if cur := store.Get(source.Key()); cur != 4 {
		t.Fatalf("cursor regressed after slow worker: %d, want 4", cur)
	}
}

func TestConsumer_StopTerminatesWithoutCtxCancel(t *testing.T) {
	src := &memSrc{}
	source, _ := startEventServer(t, src)

	cfg := ConsumerConfig{
		ProcessFunc:       func(ctx context.Context, _ Source, _ eventstream.Event) error { return nil },
		WorkerCount:       2,
		QueueSize:         8,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       &cursor.MemoryStore{},
		URLFunc:           DefaultURL(true),
		Logger:            slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	c := NewConsumer(cfg)

	c.Start(context.Background())
	c.AddSource(context.Background(), source)

	done := make(chan struct{})
	go func() {
		c.Stop()
		close(done)
	}()

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Stop did not return within 5s")
	}
}

func TestConsumer_ResumesFromStoredCursor(t *testing.T) {
	src := &memSrc{}
	for i := range 5 {
		src.add(mkEv(i))
	}

	source, _ := startEventServer(t, src)

	store := &cursor.MemoryStore{}
	store.Set(source.Key(), 3)

	seenMu := sync.Mutex{}
	seen := []int64{}

	cfg := ConsumerConfig{
		ProcessFunc: func(ctx context.Context, _ Source, msg eventstream.Event) error {
			seenMu.Lock()
			seen = append(seen, msg.Created)
			seenMu.Unlock()
			return nil
		},
		WorkerCount:       1,
		QueueSize:         16,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       store,
		URLFunc:           DefaultURL(true),
		Logger:            slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	c := NewConsumer(cfg)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	c.Start(ctx)
	c.AddSource(ctx, source)

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		seenMu.Lock()
		n := len(seen)
		seenMu.Unlock()
		if n >= 2 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	seenMu.Lock()
	defer seenMu.Unlock()
	if len(seen) < 2 {
		t.Fatalf("processed %d events, want 2: %v", len(seen), seen)
	}
	if seen[0] != 4 || seen[1] != 5 {
		t.Fatalf("resumed events = %v, want [4 5]", seen)
	}
}
