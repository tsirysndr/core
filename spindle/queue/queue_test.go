package queue

import (
	"sync"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

const (
	alice = syntax.DID("did:plc:alice")
	eve   = syntax.DID("did:plc:eve")
	dawn  = syntax.DID("did:plc:dawn")
)

func TestQueueDrainsAllJobs(t *testing.T) {
	t.Parallel()

	q := NewQueue(10, 2)
	q.Start()

	var mu sync.Mutex
	var ran []string
	done := make(chan struct{}, 5)

	for _, name := range []string{"a", "b", "c", "d", "e"} {
		q.Enqueue(dawn, Job{Run: func() error {
			mu.Lock()
			ran = append(ran, name)
			mu.Unlock()
			done <- struct{}{}
			return nil
		}})
	}

	for range 5 {
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Fatal("timed out waiting for jobs to finish")
		}
	}

	q.Stop()

	if len(ran) != 5 {
		t.Fatalf("expected 5 jobs, ran %d", len(ran))
	}
}

func TestQueueRejectsWhenFull(t *testing.T) {
	t.Parallel()

	// no workers, so the queue never drains
	q := NewQueue(2, 0)

	if !q.Enqueue(dawn, Job{Run: func() error { return nil }}) {
		t.Fatal("first Enqueue() returned false, want true")
	}
	if !q.Enqueue(dawn, Job{Run: func() error { return nil }}) {
		t.Fatal("second Enqueue() returned false, want true")
	}
	if q.Enqueue(dawn, Job{Run: func() error { return nil }}) {
		t.Fatal("third Enqueue() returned true on full queue, want false")
	}
}

func TestQueuePrefersOwnerWithFewestRunning(t *testing.T) {
	t.Parallel()

	// 2 workers. alice gets the first slot; while she's holding it, eve's
	// job should win the second slot over alice's own queued waiters.
	q := NewQueue(20, 2)

	releaseAlice1 := make(chan struct{})
	gotEve := make(chan struct{}, 1)

	q.Enqueue(alice, Job{Run: func() error {
		<-releaseAlice1
		return nil
	}})
	// alice queues two more
	q.Enqueue(alice, Job{Run: func() error { return nil }})
	q.Enqueue(alice, Job{Run: func() error { return nil }})
	// eve queues one
	q.Enqueue(eve, Job{Run: func() error {
		gotEve <- struct{}{}
		return nil
	}})

	q.Start()

	// eve should run while alice's first is still held
	select {
	case <-gotEve:
	case <-time.After(time.Second):
		t.Fatal("eve's job did not run while alice was blocked")
	}

	close(releaseAlice1)
	q.Stop()
}
