package engine

import (
	"context"
	"errors"
	"testing"
	"time"

	"tangled.org/core/spindle/models"
)

func TestSemaphoreSlotterDisabledDoesNotBlock(t *testing.T) {
	t.Parallel()

	slotter := NewSemaphoreSlotter(0)

	for range 10 {
		slot, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, Wait)
		if err != nil {
			t.Fatalf("AcquireWorkflowSlot() error = %v", err)
		}
		slot.Release()
	}
}

func TestSemaphoreSlotterBlocksUntilRelease(t *testing.T) {
	t.Parallel()

	slotter := NewSemaphoreSlotter(1)

	first, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, Wait)
	if err != nil {
		t.Fatalf("first AcquireWorkflowSlot() error = %v", err)
	}
	releasedFirst := false
	defer func() {
		if !releasedFirst {
			first.Release()
		}
	}()

	acquired := make(chan WorkflowSlot, 1)
	errs := make(chan error, 1)
	go func() {
		slot, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, Wait)
		if err != nil {
			errs <- err
			return
		}
		acquired <- slot
	}()

	assertNotAcquired(t, acquired, errs)

	first.Release()
	releasedFirst = true

	second := waitForSlot(t, acquired, errs)
	second.Release()
}

func TestSemaphoreSlotterHonorsContextCancellation(t *testing.T) {
	t.Parallel()

	slotter := NewSemaphoreSlotter(1)

	first, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, Wait)
	if err != nil {
		t.Fatalf("first AcquireWorkflowSlot() error = %v", err)
	}
	defer first.Release()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	_, err = slotter.AcquireWorkflowSlot(ctx, zeroWorkflowID(), nil, Wait)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("AcquireWorkflowSlot() error = %v, want context.Canceled", err)
	}
}

func TestSemaphoreSlotterTryDisabledDoesNotReject(t *testing.T) {
	t.Parallel()

	slotter := NewSemaphoreSlotter(0)

	for range 10 {
		slot, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, NoWait)
		if err != nil {
			t.Fatalf("AcquireWorkflowSlot(NoWait) error = %v", err)
		}
		slot.Release()
	}
}

func TestSemaphoreSlotterTryRejectsWhenFull(t *testing.T) {
	t.Parallel()

	slotter := NewSemaphoreSlotter(1)

	first, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, NoWait)
	if err != nil {
		t.Fatalf("first AcquireWorkflowSlot(NoWait) error = %v", err)
	}

	if _, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, NoWait); !errors.Is(err, ErrNoWorkflowSlots) {
		t.Fatalf("AcquireWorkflowSlot(NoWait) error = %v, want ErrNoWorkflowSlots", err)
	}

	first.Release()

	second, err := slotter.AcquireWorkflowSlot(context.Background(), zeroWorkflowID(), nil, NoWait)
	if err != nil {
		t.Fatalf("AcquireWorkflowSlot(NoWait) after release error = %v", err)
	}
	second.Release()
}

func assertNotAcquired(t *testing.T, acquired <-chan WorkflowSlot, errs <-chan error) {
	t.Helper()

	select {
	case slot := <-acquired:
		slot.Release()
		t.Fatal("AcquireWorkflowSlot() acquired a slot before one was released")
	case err := <-errs:
		t.Fatalf("AcquireWorkflowSlot() returned unexpected error: %v", err)
	case <-time.After(25 * time.Millisecond):
	}
}

func waitForSlot(t *testing.T, acquired <-chan WorkflowSlot, errs <-chan error) WorkflowSlot {
	t.Helper()

	select {
	case slot := <-acquired:
		return slot
	case err := <-errs:
		t.Fatalf("AcquireWorkflowSlot() returned error: %v", err)
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for slot acquisition")
	}

	return nil
}

func zeroWorkflowID() models.WorkflowId {
	return models.WorkflowId{}
}
