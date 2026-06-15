package knotacl

import (
	"context"
	"errors"
	"path/filepath"
	"slices"
	"sync"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

var rosterTestBase = time.Unix(1700000000, 0)

const rosterTTL = 5 * time.Minute

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

func (f *fakeLister) arm(started, block chan struct{}) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.started, f.block = started, block
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

func rosterTestDB(t *testing.T) *db.DB {
	t.Helper()
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "appview.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

const rosterHost = "knot.nel.pet"

func membersForHost(t *testing.T, d *db.DB, host string) []models.KnotMember {
	t.Helper()
	rows, err := db.GetKnotMembers(d, orm.FilterEq("domain", host))
	if err != nil {
		t.Fatalf("GetKnotMembers: %v", err)
	}
	return rows
}

func TestRoster_BootstrapThenServesFromSqlite(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	got, err := r.GetKnotMembers(ctx, rosterHost)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(got, []string{"did:plc:boltless"}) {
		t.Errorf("bootstrap read = %v, want the drained roster", got)
	}

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; a read within the reconcile TTL must be served from sqlite", f.calls())
	}

	clk.advance(rosterTTL)
	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 2 {
		t.Errorf("memberCalls=%d, want 2; a stale scope must reconcile from XRPC", f.calls())
	}
}

func TestRoster_EventDeltaVisibleWithoutXRPC(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	d := rosterTestDB(t)
	r := newRoster(d, f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:akshay"), 1); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}

	got, err := r.GetKnotMembers(ctx, rosterHost)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(got, []string{"did:plc:akshay", "did:plc:boltless"}) {
		t.Errorf("post-delta read = %v, want the pushed member reflected", got)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; a pushed delta must not trigger an XRPC reconcile", f.calls())
	}
}

func TestRoster_ColdUnreachableErrors(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{err: errors.New("knot unreachable")}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)

	if _, err := r.GetKnotMembers(context.Background(), rosterHost); !errors.Is(err, ErrKnotUnreachable) {
		t.Fatalf("err=%v, want ErrKnotUnreachable when cold and the bootstrap drain fails", err)
	}
}

func TestRoster_StaleServedWhenUnreachable(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}

	clk.advance(2 * rosterTTL)
	f.set(nil, errors.New("knot unreachable"))

	got, err := r.GetKnotMembers(ctx, rosterHost)
	if err != nil {
		t.Fatalf("err=%v, want stale rows served when a once-synced scope goes unreachable", err)
	}
	if !slices.Equal(got, []string{"did:plc:boltless"}) {
		t.Errorf("stale read = %v, want the last good roster", got)
	}
}

func TestRoster_InvalidateForcesReconcile(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}
	r.InvalidateMembers(rosterHost)

	f.set([]string{"did:plc:akshay"}, nil)
	got, err := r.GetKnotMembers(ctx, rosterHost)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(got, []string{"did:plc:akshay"}) {
		t.Errorf("post-invalidate read = %v, want a fresh reconcile within the TTL", got)
	}
	if f.calls() != 2 {
		t.Errorf("memberCalls=%d, want 2; invalidation must force the next read to reconcile", f.calls())
	}
}

func TestRoster_SingleflightCollapsesConcurrentColdReads(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	started := make(chan struct{})
	release := make(chan struct{})
	f := &fakeLister{members: []string{"did:plc:boltless"}, started: started, block: release}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	var wg sync.WaitGroup
	call := func() {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
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
		t.Errorf("memberCalls=%d, want 1; concurrent cold reads must collapse into a single reconcile", f.calls())
	}
}

func TestRoster_MemberDeltaIdempotentAndScoped(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 1); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}
	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 2); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}
	if err := r.RemoveKnotMember("other.nel.pet", syntax.DID("did:plc:boltless"), 1); err != nil {
		t.Fatalf("RemoveKnotMember other host: %v", err)
	}

	if rows := membersForHost(t, d, rosterHost); len(rows) != 1 {
		t.Fatalf("members = %v, want a single row after a duplicate add and an unrelated-host remove", rows)
	}

	if err := r.RemoveKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 3); err != nil {
		t.Fatalf("RemoveKnotMember: %v", err)
	}
	if rows := membersForHost(t, d, rosterHost); len(rows) != 0 {
		t.Fatalf("members = %v, want empty after remove", rows)
	}
}

func TestRoster_NativeAddReplacesLegacyRow(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)

	if err := db.AddKnotMember(d, models.KnotMember{
		Did:     syntax.DID("did:plc:akshay"),
		Rkey:    "legacy-rkey",
		Domain:  rosterHost,
		Subject: syntax.DID("did:plc:boltless"),
	}); err != nil {
		t.Fatalf("seed legacy row: %v", err)
	}

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 1); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}

	rows := membersForHost(t, d, rosterHost)
	if len(rows) != 1 {
		t.Fatalf("members = %v, want a single canonical row after a native add over a legacy row", rows)
	}
	if rows[0].Did != "" {
		t.Errorf("row did = %q, want empty; the native write must own the (domain, subject) identity", rows[0].Did)
	}
}

func TestRoster_StaleDeltaIgnored(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 100); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}
	if err := r.RemoveKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 50); err != nil {
		t.Fatalf("RemoveKnotMember: %v", err)
	}

	if rows := membersForHost(t, d, rosterHost); len(rows) != 1 {
		t.Fatalf("members = %v, want the add preserved; a lower-cursor remove arriving late must be ignored", rows)
	}
}

func TestRoster_NewerRemoveWins(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 100); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}
	if err := r.RemoveKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 101); err != nil {
		t.Fatalf("RemoveKnotMember: %v", err)
	}

	if rows := membersForHost(t, d, rosterHost); len(rows) != 0 {
		t.Fatalf("members = %v, want empty; a higher-cursor remove must win", rows)
	}
}

func TestRoster_LateAddAfterRemoveIgnored(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)

	if err := r.RemoveKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 101); err != nil {
		t.Fatalf("RemoveKnotMember: %v", err)
	}
	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:boltless"), 100); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}

	if rows := membersForHost(t, d, rosterHost); len(rows) != 0 {
		t.Fatalf("members = %v, want empty; an add older than the applied remove must be ignored", rows)
	}
}

func TestRoster_ReconcilePrunesStaleCursors(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	d := rosterTestDB(t)
	r := newRoster(d, &fakeLister{}, rosterTTL, clk.now, nil)
	ctx := context.Background()

	stale := Cursor(rosterTestBase.Add(-2 * cursorRetention).UnixNano())
	fresh := Cursor(rosterTestBase.UnixNano())

	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:clam"), stale); err != nil {
		t.Fatalf("AddKnotMember stale: %v", err)
	}
	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:whelk"), fresh); err != nil {
		t.Fatalf("AddKnotMember fresh: %v", err)
	}

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatalf("reconcile: %v", err)
	}

	scope := memberScope(rosterHost)
	if _, ok, err := seenCursor(d, scope, syntax.DID("did:plc:clam")); err != nil || ok {
		t.Errorf("stale cursor present (ok=%v err=%v); reconcile must prune cursors older than the retention window", ok, err)
	}
	if _, ok, err := seenCursor(d, scope, syntax.DID("did:plc:whelk")); err != nil || !ok {
		t.Errorf("fresh cursor missing (ok=%v err=%v); reconcile must keep cursors within the retention window", ok, err)
	}
}

func TestRoster_ReconcileDoesNotClobberConcurrentDelta(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}

	clk.advance(rosterTTL)

	started := make(chan struct{})
	release := make(chan struct{})
	f.arm(started, release)

	done := make(chan []string, 1)
	go func() {
		got, err := r.GetKnotMembers(ctx, rosterHost)
		if err != nil {
			t.Errorf("reconcile read: %v", err)
		}
		done <- got
	}()

	<-started
	if err := r.AddKnotMember(rosterHost, syntax.DID("did:plc:akshay"), 1); err != nil {
		t.Fatalf("AddKnotMember: %v", err)
	}
	close(release)

	got := <-done
	if !slices.Contains(got, "did:plc:akshay") {
		t.Errorf("post-reconcile read = %v, want the delta that landed during the drain preserved", got)
	}
}

func TestRoster_BackoffSuppressesRepeatedDrains(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{members: []string{"did:plc:boltless"}}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 1 {
		t.Fatalf("memberCalls=%d, want 1 after bootstrap", f.calls())
	}

	clk.advance(rosterTTL)
	f.set(nil, errors.New("knot unreachable"))

	if got, err := r.GetKnotMembers(ctx, rosterHost); err != nil || !slices.Equal(got, []string{"did:plc:boltless"}) {
		t.Fatalf("got %v err %v, want the stale roster served", got, err)
	}
	if f.calls() != 2 {
		t.Fatalf("memberCalls=%d, want 2 after the first stale drain", f.calls())
	}

	if got, err := r.GetKnotMembers(ctx, rosterHost); err != nil || !slices.Equal(got, []string{"did:plc:boltless"}) {
		t.Fatalf("got %v err %v, want the stale roster served", got, err)
	}
	if f.calls() != 2 {
		t.Errorf("memberCalls=%d, want 2; a down knot must not be re-probed within the backoff window", f.calls())
	}

	clk.advance(reconcileBackoff)
	if _, err := r.GetKnotMembers(ctx, rosterHost); err != nil {
		t.Fatal(err)
	}
	if f.calls() != 3 {
		t.Errorf("memberCalls=%d, want 3; the probe must resume once the backoff window elapses", f.calls())
	}
}

func TestRoster_BackoffStillErrorsWhenCold(t *testing.T) {
	clk := &fakeClock{t: rosterTestBase}
	f := &fakeLister{err: errors.New("knot unreachable")}
	r := newRoster(rosterTestDB(t), f, rosterTTL, clk.now, nil)
	ctx := context.Background()

	if _, err := r.GetKnotMembers(ctx, rosterHost); !errors.Is(err, ErrKnotUnreachable) {
		t.Fatalf("err=%v, want ErrKnotUnreachable on the cold drain failure", err)
	}
	if f.calls() != 1 {
		t.Fatalf("memberCalls=%d, want 1", f.calls())
	}

	if _, err := r.GetKnotMembers(ctx, rosterHost); !errors.Is(err, ErrKnotUnreachable) {
		t.Fatalf("err=%v, want ErrKnotUnreachable during backoff for a cold scope", err)
	}
	if f.calls() != 1 {
		t.Errorf("memberCalls=%d, want 1; a cold scope must not be re-probed within the backoff window", f.calls())
	}
}
