package jetstream

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/bluesky-social/jetstream/pkg/models"
)

type fakeCursorDB struct {
	saved    int64
	savedErr error
}

func (f *fakeCursorDB) GetLastTimeUs() (int64, error) { return f.saved, f.savedErr }
func (f *fakeCursorDB) SaveLastTimeUs(int64) error    { return nil }

func cursorFor(db DB) int64 {
	j := &JetstreamClient{db: db, wantedDids: make(Set[string])}
	return *j.getLastTimeUs(context.Background())
}

const twoDaysUs = int64(2 * 24 * 60 * 60 * 1000 * 1000)

func TestStaleCursorIsHonored(t *testing.T) {
	old := time.Now().UnixMicro() - 5*twoDaysUs
	if got := cursorFor(&fakeCursorDB{saved: old}); got != old {
		t.Fatalf("stale cursor must be honored, not snapped forward: got %d, want %d", got, old)
	}
}

func TestZeroCursorIsHonored(t *testing.T) {
	if got := cursorFor(&fakeCursorDB{saved: 0}); got != 0 {
		t.Fatalf("cursor 0 must replay from the start: got %d, want 0", got)
	}
}

func TestMissingCursorStartsFromNow(t *testing.T) {
	before := time.Now().UnixMicro()
	got := cursorFor(&fakeCursorDB{savedErr: errors.New("no row")})
	after := time.Now().UnixMicro()
	if got < before || got > after {
		t.Fatalf("missing cursor must start from now: got %d, want within [%d,%d]", got, before, after)
	}
}

func TestLastSeenTracksEveryEventPreFilter(t *testing.T) {
	j := &JetstreamClient{wantedDids: Set[string]{"did:plc:boltless": {}}}
	wrapped := j.withDidFilter(func(context.Context, *models.Event) error { return nil })

	_ = wrapped(context.Background(), &models.Event{Did: "did:plc:akshay", TimeUS: 100})
	if got := j.lastSeenUs.Load(); got != 101 {
		t.Fatalf("filtered-out event must still advance last-seen: got %d, want 101", got)
	}

	_ = wrapped(context.Background(), &models.Event{Did: "did:plc:boltless", TimeUS: 200})
	if got := j.lastSeenUs.Load(); got != 201 {
		t.Fatalf("matching event must advance last-seen: got %d, want 201", got)
	}
}

func TestLastSeenAdvancesAfterProcessing(t *testing.T) {
	j := &JetstreamClient{wantedDids: make(Set[string])}

	var seenDuringProcess int64
	wrapped := j.withDidFilter(func(context.Context, *models.Event) error {
		seenDuringProcess = j.lastSeenUs.Load()
		return nil
	})

	_ = wrapped(context.Background(), &models.Event{Did: "did:plc:boltless", TimeUS: 500})

	if seenDuringProcess == 501 {
		t.Fatal("cursor advanced before the event finished processing; a crash mid-process would skip it")
	}
	if got := j.lastSeenUs.Load(); got != 501 {
		t.Fatalf("cursor must advance once processing returns: got %d, want 501", got)
	}
}
