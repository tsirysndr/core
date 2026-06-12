package engine

import (
	"context"
	"errors"
	"fmt"
	"testing"
	"time"
)

// resources for testing
type ru struct{ a, b int64 }

func (r ru) Fits(limit ru) bool {
	if limit.a > 0 && r.a > limit.a {
		return false
	}
	if limit.b > 0 && r.b > limit.b {
		return false
	}
	return true
}
func (r ru) Add(o ru) ru { return ru{r.a + o.a, r.b + o.b} }
func (r ru) Sub(o ru) ru { return ru{max(0, r.a-o.a), max(0, r.b-o.b)} }
func (r ru) String() string {
	return fmt.Sprintf("a=%d b=%d", r.a, r.b)
}

type acquireResult struct {
	slot WorkflowSlot
	err  error
}

func TestResourceSchedulerZeroLimitsDoNotApply(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{}, ru{}, 0)

	slot, err := scheduler.Acquire(context.Background(), ru{a: 1 << 20, b: 1 << 20})
	if err != nil {
		t.Fatalf("Acquire() error = %v", err)
	}
	slot.Release()
}

func TestResourceSchedulerRejectsRequestsThatCanNeverFit(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{a: 1024, b: 10_000}, ru{a: 512, b: 5_000}, 0)

	_, err := scheduler.Acquire(context.Background(), ru{a: 768, b: 100})
	if !errors.Is(err, ErrNoWorkflowSlots) {
		t.Fatalf("Acquire() error = %v, want ErrNoWorkflowSlots", err)
	}

	_, err = scheduler.Acquire(context.Background(), ru{a: 128, b: 12_000})
	if !errors.Is(err, ErrNoWorkflowSlots) {
		t.Fatalf("Acquire() error = %v, want ErrNoWorkflowSlots", err)
	}
}

func TestResourceSchedulerWaitsUntilResourcesAreReleased(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{a: 1024}, ru{}, 0)

	first, err := scheduler.Acquire(context.Background(), ru{a: 1024})
	if err != nil {
		t.Fatalf("first Acquire() error = %v", err)
	}
	defer first.Release()

	ch := acquireAsync(context.Background(), scheduler, ru{a: 1})
	assertAcquireBlocked(t, ch)

	first.Release()
	first = NoopSlot{}

	second := waitAcquireOK(t, ch)
	second.Release()
}

func TestResourceSchedulerReleaseIsIdempotent(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{a: 1}, ru{}, 0)

	slot, err := scheduler.Acquire(context.Background(), ru{a: 1})
	if err != nil {
		t.Fatalf("Acquire() error = %v", err)
	}

	slot.Release()
	slot.Release()

	second, err := scheduler.Acquire(context.Background(), ru{a: 1})
	if err != nil {
		t.Fatalf("Acquire() after double release error = %v", err)
	}
	second.Release()
}

func TestResourceSchedulerBackfillsPastBlockedHead(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{a: 1024}, ru{}, time.Hour) // disable aging so we test pure backfill

	hold, err := scheduler.Acquire(context.Background(), ru{a: 512})
	if err != nil {
		t.Fatalf("hold Acquire() error = %v", err)
	}
	defer hold.Release()

	bigCh := acquireAsync(context.Background(), scheduler, ru{a: 768})
	assertAcquireBlocked(t, bigCh)

	smallCh := acquireAsync(context.Background(), scheduler, ru{a: 256})
	small := waitAcquireOK(t, smallCh)
	small.Release()

	assertAcquireBlocked(t, bigCh)
}

func TestResourceSchedulerAgingReservesCapacityForBlockedHead(t *testing.T) {
	t.Parallel()

	scheduler := NewResourceScheduler(ru{a: 1024}, ru{}, 10*time.Millisecond)
	fakeNow := time.Now()
	scheduler.now = func() time.Time { return fakeNow }

	hold, err := scheduler.Acquire(context.Background(), ru{a: 512})
	if err != nil {
		t.Fatalf("hold Acquire() error = %v", err)
	}

	bigCh := acquireAsync(context.Background(), scheduler, ru{a: 768})
	assertAcquireBlocked(t, bigCh)

	fakeNow = fakeNow.Add(time.Second)

	// big is now aged and reserves its 768. a 256 request would fit
	// alongside the held 512, but the reservation blocks it.
	smallCh := acquireAsync(context.Background(), scheduler, ru{a: 256})
	assertAcquireBlocked(t, smallCh)

	hold.Release()

	big := waitAcquireOK(t, bigCh)
	small := waitAcquireOK(t, smallCh)
	small.Release()
	big.Release()
}

func acquireAsync(ctx context.Context, scheduler *ResourceScheduler[ru], req ru) <-chan acquireResult {
	ch := make(chan acquireResult, 1)
	go func() {
		slot, err := scheduler.Acquire(ctx, req)
		ch <- acquireResult{slot: slot, err: err}
	}()
	return ch
}

func assertAcquireBlocked(t *testing.T, ch <-chan acquireResult) {
	t.Helper()

	select {
	case res := <-ch:
		if res.slot != nil {
			res.slot.Release()
		}
		t.Fatalf("Acquire() returned before resources were available: err=%v", res.err)
	case <-time.After(25 * time.Millisecond):
	}
}

func waitAcquireOK(t *testing.T, ch <-chan acquireResult) WorkflowSlot {
	t.Helper()

	res := waitAcquireResult(t, ch)
	if res.err != nil {
		t.Fatalf("Acquire() error = %v", res.err)
	}
	if res.slot == nil {
		t.Fatal("Acquire() returned nil slot")
	}
	return res.slot
}

func waitAcquireResult(t *testing.T, ch <-chan acquireResult) acquireResult {
	t.Helper()

	select {
	case res := <-ch:
		return res
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for Acquire() result")
	}

	return acquireResult{}
}
