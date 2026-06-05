package knotacl

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"sync"
	"testing"
	"time"
)

var cacheTestBase = time.Unix(1700000000, 0)

type fakeLister struct {
	mu          sync.Mutex
	memberCalls int
	members     []string
	err         error
	started     chan struct{}
	block       chan struct{}
}

func (f *fakeLister) GetKnotMembers(ctx context.Context, host string) ([]string, error) {
	f.mu.Lock()
	f.memberCalls++
	members, err, started, block := f.members, f.err, f.started, f.block
	f.mu.Unlock()
	if started != nil {
		close(started)
	}
	if block != nil {
		<-block
	}
	if err != nil {
		return nil, err
	}
	return members, nil
}

func (f *fakeLister) GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.members, f.err
}

func (f *fakeLister) calls() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.memberCalls
}

func (f *fakeLister) set(members []string, err error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.members, f.err = members, err
}

type fakeClock struct {
	mu sync.Mutex
	t  time.Time
}

func (c *fakeClock) now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.t
}

func (c *fakeClock) advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.t = c.t.Add(d)
}

func TestCache_TTLCollapsesThenExpires(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	c := newCache(f, cacheTTL, clk.now)
	ctx := context.Background()

	if _, err := c.GetKnotMembers(ctx, "knot.nel.pet"); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetKnotMembers(ctx, "knot.nel.pet"); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1 within the TTL window", f.calls())
	}

	clk.advance(cacheTTL)
	if _, err := c.GetKnotMembers(ctx, "knot.nel.pet"); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 2 {
		t.Errorf("memberCalls=%d, want 2 once the entry expired", f.calls())
	}
}

func TestCache_ErrorsNotCached(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	f := &fakeLister{err: errors.New("knot unreachable")}
	c := newCache(f, cacheTTL, clk.now)
	ctx := context.Background()

	if _, err := c.GetKnotMembers(ctx, "knot.nel.pet"); err == nil {
		t.Fatal("want error on the first call")
	}
	f.set([]string{"did:plc:boltless"}, nil)
	got, err := c.GetKnotMembers(ctx, "knot.nel.pet")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(got, []string{"did:plc:boltless"}) {
		t.Errorf("got %v after recovery, want the live value", got)
	}
	if f.calls() != 2 {
		t.Errorf("memberCalls=%d, want 2; a failed fetch must not be cached", f.calls())
	}
}

func TestCache_MemoShortCircuitsWithinRequest(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	c := newCache(f, cacheTTL, clk.now)
	ctx := WithMemo(context.Background())

	first, err := c.GetKnotMembers(ctx, "knot.nel.pet")
	if err != nil {
		t.Fatal(err)
	}

	clk.advance(2 * cacheTTL)
	f.set([]string{"did:plc:akshay"}, nil)

	second, err := c.GetKnotMembers(ctx, "knot.nel.pet")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(first, second) {
		t.Errorf("memo must hold one snapshot per request: first=%v second=%v", first, second)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; the request memo must not re-query even past the TTL", f.calls())
	}
}

func TestCache_ReturnedSliceCannotCorruptCache(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless", "did:plc:akshay"}}
	c := newCache(f, cacheTTL, clk.now)
	ctx := context.Background()

	got, err := c.GetKnotMembers(ctx, "knot.nel.pet")
	if err != nil {
		t.Fatal(err)
	}
	for i := range got {
		got[i] = "did:plc:squid"
	}

	again, err := c.GetKnotMembers(ctx, "knot.nel.pet")
	if err != nil {
		t.Fatal(err)
	}
	if slices.Contains(again, "did:plc:squid") {
		t.Errorf("a caller mutating its returned slice corrupted the cached entry: %v", again)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; the second read should be served from cache", f.calls())
	}
}

func TestCache_SingleflightCollapsesConcurrentMisses(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	started := make(chan struct{})
	release := make(chan struct{})
	f := &fakeLister{members: []string{"did:plc:boltless"}, started: started, block: release}
	c := newCache(f, cacheTTL, clk.now)
	ctx := context.Background()

	var wg sync.WaitGroup
	call := func() {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, err := c.GetKnotMembers(ctx, "knot.nel.pet"); err != nil {
				t.Errorf("GetKnotMembers: %v", err)
			}
		}()
	}

	call()
	<-started
	for range make([]struct{}, 8) {
		call()
	}
	time.Sleep(20 * time.Millisecond)
	close(release)
	wg.Wait()

	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; concurrent misses must collapse into a single knot query", f.calls())
	}
}

func TestCache_CapIsHardUnderFreshFlood(t *testing.T) {
	clk := &fakeClock{t: cacheTestBase}
	f := &fakeLister{members: []string{"did:plc:limpet"}}
	c := newCache(f, cacheTTL, clk.now)
	ctx := context.Background()

	for i := 0; i < cacheMaxEntries+100; i++ {
		if _, err := c.GetKnotMembers(ctx, fmt.Sprintf("knot-%d.nel.pet", i)); err != nil {
			t.Fatal(err)
		}
	}

	c.mu.Lock()
	n := len(c.entries)
	c.mu.Unlock()
	if n > cacheMaxEntries {
		t.Errorf("entries=%d, want <= %d; all-fresh keys must not grow past the cap", n, cacheMaxEntries)
	}
}
