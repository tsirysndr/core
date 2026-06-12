package eventconsumer

import (
	"context"
	"io"
	"log/slog"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
)

func sqliteCursorStore(t *testing.T) cursor.Store {
	t.Helper()
	store, err := cursor.NewSQLiteStore(filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatalf("new sqlite cursor store: %v", err)
	}
	return store
}

func drainProcessed(t *testing.T, store cursor.Store, source Source) []int64 {
	t.Helper()

	var mu sync.Mutex
	var seen []int64

	c := NewConsumer(ConsumerConfig{
		ProcessFunc: func(_ context.Context, _ Source, msg eventstream.Event) error {
			mu.Lock()
			seen = append(seen, msg.Created)
			mu.Unlock()
			return nil
		},
		WorkerCount:       1,
		QueueSize:         16,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       store,
		Logger:            slog.New(slog.NewTextHandler(io.Discard, nil)),
	})

	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	c.Start(ctx)
	c.AddSource(ctx, source)

	deadline := time.Now().Add(3 * time.Second)
	last, stable := -1, 0
	for time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
		mu.Lock()
		n := len(seen)
		mu.Unlock()
		if n == last {
			if stable++; stable >= 3 && n > 0 {
				break
			}
		} else {
			last, stable = n, 0
		}
	}

	mu.Lock()
	defer mu.Unlock()
	return append([]int64(nil), seen...)
}

func TestSpindleUpgrade_OrphanedCursorReplaysFromZero(t *testing.T) {
	src := &memSrc{}
	for i := range 8 {
		src.add(mkEv(i))
	}
	source, _ := startEventServer(t, src)

	store := sqliteCursorStore(t)
	store.Set(source.Host, 5)

	seen := drainProcessed(t, store, source)

	if len(seen) != 8 {
		t.Fatalf("orphaned bare-host cursor processed %d events, want a full replay of 8: %v", len(seen), seen)
	}
}

func TestSpindleUpgrade_MigratedCursorResumesNoReplay(t *testing.T) {
	src := &memSrc{}
	for i := range 8 {
		src.add(mkEv(i))
	}
	source, _ := startEventServer(t, src)

	store := sqliteCursorStore(t)
	store.Set(source.Host, 5)

	MigrateLegacyCursor(store, source)

	seen := drainProcessed(t, store, source)

	if len(seen) != 3 {
		t.Fatalf("migrated cursor processed %d events, want a resume of 3: %v", len(seen), seen)
	}
	if seen[0] != 6 || seen[2] != 8 {
		t.Fatalf("resumed events = %v, want [6 7 8]", seen)
	}
}
