package knotcompat

import (
	"strconv"
	"sync"
	"testing"
	"time"
)

type fakeLatch struct {
	mu     sync.Mutex
	native map[string]bool
	marks  []string
	reads  int
}

func newFakeLatch() *fakeLatch {
	return &fakeLatch{native: map[string]bool{}}
}

func (f *fakeLatch) IsNative(host string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.reads++
	return f.native[host]
}

func (f *fakeLatch) readCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.reads
}

func (f *fakeLatch) MarkNative(host string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.native[host] = true
	f.marks = append(f.marks, host)
}

func (f *fakeLatch) markCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.marks)
}

func probeReturning(s CapStatus, calls *int) func() CapStatus {
	return func() CapStatus {
		*calls++
		return s
	}
}

func isNativeForTest(g *nativeGate, host string, probe func() CapStatus) bool {
	return g.status(host, probe) == CapPresent
}

func TestNativeGateMemoSkipsSecondProbe(t *testing.T) {
	g := &nativeGate{}
	calls := 0
	probe := probeReturning(CapPresent, &calls)

	if !isNativeForTest(g, "clam.nel.pet", probe) {
		t.Fatal("first probe true: want native")
	}
	if !isNativeForTest(g, "clam.nel.pet", probe) {
		t.Fatal("memoized: want native")
	}
	if calls != 1 {
		t.Fatalf("calls = %d, want 1; a memoized native host must skip the probe", calls)
	}
}

func TestNativeGateLatchHitSkipsProbe(t *testing.T) {
	g := &nativeGate{}
	fl := newFakeLatch()
	fl.native["whelk.nel.pet"] = true
	g.use(fl)

	calls := 0
	if !isNativeForTest(g, "whelk.nel.pet", probeReturning(CapAbsent, &calls)) {
		t.Fatal("latched native: want native even though the probe would fail")
	}
	if calls != 0 {
		t.Fatalf("calls = %d, want 0; a latched host must never be probed", calls)
	}
	if fl.markCount() != 0 {
		t.Fatalf("marks = %d, want 0; a latch hit must not re-mark", fl.markCount())
	}
}

func TestNativeGateProbeMarksLatchOnce(t *testing.T) {
	g := &nativeGate{}
	fl := newFakeLatch()
	g.use(fl)

	calls := 0
	probe := probeReturning(CapPresent, &calls)
	if !isNativeForTest(g, "limpet.nel.pet", probe) {
		t.Fatal("probe true: want native")
	}
	if fl.markCount() != 1 {
		t.Fatalf("marks = %d, want 1; a first successful probe must latch the host", fl.markCount())
	}

	isNativeForTest(g, "limpet.nel.pet", probe)
	if fl.markCount() != 1 {
		t.Fatalf("marks = %d, want 1; the memo must prevent a second mark", fl.markCount())
	}
}

func TestNativeGateProbeFalseDoesNotMark(t *testing.T) {
	g := &nativeGate{}
	fl := newFakeLatch()
	g.use(fl)

	calls := 0
	if isNativeForTest(g, "clam.nel.pet", probeReturning(CapAbsent, &calls)) {
		t.Fatal("probe false on a fresh host: want not native")
	}
	if fl.markCount() != 0 {
		t.Fatalf("marks = %d, want 0; a failed probe must not latch", fl.markCount())
	}
}

func TestNativeGateDurableAcrossMemoReset(t *testing.T) {
	fl := newFakeLatch()

	warm := &nativeGate{}
	warm.use(fl)
	calls := 0
	if !isNativeForTest(warm, "whelk.nel.pet", probeReturning(CapPresent, &calls)) {
		t.Fatal("warm gate probe true: want native")
	}

	restarted := &nativeGate{}
	restarted.use(fl)
	cold := 0
	if !isNativeForTest(restarted, "whelk.nel.pet", probeReturning(CapAbsent, &cold)) {
		t.Fatal("after restart the durable latch must resolve native without a probe")
	}
	if cold != 0 {
		t.Fatalf("calls = %d, want 0; a durably latched host survives a memo reset without probing", cold)
	}
}

func TestNativeGateNegativeMemoThrottlesLatchReads(t *testing.T) {
	now := time.Now()
	g := &nativeGate{now: func() time.Time { return now }}
	fl := newFakeLatch()
	g.use(fl)

	calls := 0
	probe := probeReturning(CapAbsent, &calls)

	for range 5 {
		if isNativeForTest(g, "clam.nel.pet", probe) {
			t.Fatal("a probe-false host must not be native")
		}
	}
	if fl.readCount() != 1 {
		t.Fatalf("latch reads = %d, want 1; the negative memo must throttle repeat latch reads within the window", fl.readCount())
	}
	if calls != 1 {
		t.Fatalf("probe calls = %d, want 1; the negative memo must throttle repeat probes within the window", calls)
	}

	now = now.Add(versionProbeFresh + time.Second)
	if isNativeForTest(g, "clam.nel.pet", probe) {
		t.Fatal("still not native after the window")
	}
	if fl.readCount() != 2 {
		t.Fatalf("latch reads = %d, want 2; an expired negative memo must re-read the latch", fl.readCount())
	}
	if calls != 2 {
		t.Fatalf("probe calls = %d, want 2; an expired negative memo must re-probe", calls)
	}
}

func TestNativeGateNegativeMemoNeverShadowsLatchedNative(t *testing.T) {
	now := time.Now()
	g := &nativeGate{now: func() time.Time { return now }}
	fl := newFakeLatch()
	g.use(fl)

	calls := 0
	if isNativeForTest(g, "whelk.nel.pet", probeReturning(CapAbsent, &calls)) {
		t.Fatal("probe false on a fresh host: want not native")
	}

	fl.mu.Lock()
	fl.native["whelk.nel.pet"] = true
	fl.mu.Unlock()

	now = now.Add(versionProbeFresh + time.Second)
	if !isNativeForTest(g, "whelk.nel.pet", probeReturning(CapAbsent, &calls)) {
		t.Fatal("once the negative memo expires a latched host must resolve native again")
	}
}

func TestNativeGateUnknownDistinctFromAbsent(t *testing.T) {
	now := time.Now()
	g := &nativeGate{now: func() time.Time { return now }}
	fl := newFakeLatch()
	g.use(fl)

	calls := 0
	probe := probeReturning(CapUnknown, &calls)

	for range 3 {
		if got := g.status("clam.nel.pet", probe); got != CapUnknown {
			t.Fatalf("status = %v, want CapUnknown for a failed probe", got)
		}
	}
	if calls != 1 {
		t.Fatalf("probe calls = %d, want 1; a memoized unknown must throttle re-probes within the window", calls)
	}
	if fl.markCount() != 0 {
		t.Fatalf("marks = %d, want 0; an unknown probe must never latch", fl.markCount())
	}

	now = now.Add(versionProbeFresh + time.Second)
	if got := g.status("clam.nel.pet", probeReturning(CapAbsent, &calls)); got != CapAbsent {
		t.Fatalf("status = %v, want CapAbsent once a fresh probe reaches the knot", got)
	}
}

func TestAtLeast(t *testing.T) {
	cases := []struct {
		in       string
		minMajor int
		minMinor int
		want     bool
	}{
		{"v1.14.0", 1, 14, true},
		{"v1.14.0-alpha", 1, 14, true},
		{"v1.14.5", 1, 14, true},
		{"v1.13.0", 1, 14, false},
		{"v1.13.0-alpha", 1, 14, false},
		{"v1.0.0", 1, 14, false},
		{"v2.0.0", 1, 14, true},
		{"1.14.0", 1, 14, true},
		{"1.13.99", 1, 14, false},
		{"(devel)", 1, 14, true},
		{"", 1, 14, false},
		{"garbagio-furioso", 1, 14, false},
		{"v1", 1, 14, false},
		{"vX.Y.Z", 1, 14, false},
		{"unknown", 1, 14, false},
		{"unknown-abc1234", 1, 14, false},
		{"unknown-abc1234-modified", 1, 14, false},
		{"v1.15.0", 1, 15, true},
		{"v1.15.2-alpha", 1, 15, true},
		{"v1.16.0", 1, 15, true},
		{"v1.14.1-alpha", 1, 15, false},
		{"v1.14.9", 1, 15, false},
		{"v2.0.0", 1, 15, true},
		{"(devel)", 1, 15, true},
		{"", 1, 15, false},
	}
	for _, c := range cases {
		t.Run(c.in, func(t *testing.T) {
			if got := atLeast(c.in, c.minMajor, c.minMinor); got != c.want {
				t.Errorf("atLeast(%q, %d, %d) = %v, want %v", c.in, c.minMajor, c.minMinor, got, c.want)
			}
		})
	}
}

func newProbeCache() *versionProbeCache {
	return &versionProbeCache{entries: map[string]versionProbeEntry{}}
}

func TestVersionProbeCacheFreshSkipsProbe(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	calls := 0
	probe := func() (string, bool) { calls++; return "v1.15.0", true }

	if !c.supports(now, "knot.nel.pet", 1, 15, false, probe) {
		t.Fatal("cold probe: want supported")
	}
	if calls != 1 {
		t.Fatalf("calls = %d, want 1 after cold probe", calls)
	}
	if !c.supports(now.Add(time.Minute), "knot.nel.pet", 1, 15, false, probe) {
		t.Fatal("cached: want supported")
	}
	if calls != 1 {
		t.Fatalf("calls = %d, want 1; a fresh cache entry must skip the probe", calls)
	}
}

func TestVersionProbeCacheServesStaleOnFailure(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	if !c.supports(now, "knot.nel.pet", 1, 15, false, func() (string, bool) { return "v1.15.0", true }) {
		t.Fatal("seed: want supported")
	}

	failProbe := func() (string, bool) { return "", false }
	if !c.supports(now.Add(10*time.Minute), "knot.nel.pet", 1, 15, false, failProbe) {
		t.Error("a probe failure within the trust window must serve the last-known version, not fail closed")
	}
}

func TestVersionProbeCacheFailsClosedWhenUntrusted(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	failProbe := func() (string, bool) { return "", false }
	if c.supports(now, "knot.nel.pet", 1, 15, false, failProbe) {
		t.Error("a cold probe failure on a fail-closed gate must return false")
	}
	if !c.supports(now, "knot.nel.pet", 1, 14, true, failProbe) {
		t.Error("a cold probe failure on a fail-open gate must return true")
	}
}

func TestVersionProbeCacheExpiresTrust(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	if !c.supports(now, "knot.nel.pet", 1, 15, false, func() (string, bool) { return "v1.15.0", true }) {
		t.Fatal("seed: want supported")
	}
	if c.supports(now.Add(2*time.Hour), "knot.nel.pet", 1, 15, false, func() (string, bool) { return "", false }) {
		t.Error("a probe failure past the trust window must fail closed")
	}
}

func TestVersionProbeCacheRefreshesAfterFresh(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	if c.supports(now, "knot.nel.pet", 1, 15, false, func() (string, bool) { return "v1.14.0", true }) {
		t.Fatal("seed: 1.14 must not satisfy 1.15")
	}
	if !c.supports(now.Add(10*time.Minute), "knot.nel.pet", 1, 15, false, func() (string, bool) { return "v1.15.0", true }) {
		t.Error("a re-probe past the fresh window must pick up the upgraded version")
	}
}

func TestVersionProbeCacheEnforcesHardCap(t *testing.T) {
	c := newProbeCache()
	now := time.Unix(1_000_000, 0)
	probe := func() (string, bool) { return "v1.15.0", true }
	for i := 0; i < versionProbeCacheMax+200; i++ {
		c.supports(now, "knot"+strconv.Itoa(i)+".nel.pet", 1, 15, false, probe)
	}
	if len(c.entries) > versionProbeCacheMax {
		t.Fatalf("entries = %d, must never exceed cap %d even when every entry is fresh", len(c.entries), versionProbeCacheMax)
	}
}

func TestVersionProbeCacheEvictsOldestWhenFull(t *testing.T) {
	c := newProbeCache()
	base := time.Unix(1_000_000, 0)
	probe := func() (string, bool) { return "v1.15.0", true }
	for i := 0; i < versionProbeCacheMax; i++ {
		c.supports(base.Add(time.Duration(i)*time.Millisecond), "knot"+strconv.Itoa(i)+".nel.pet", 1, 15, false, probe)
	}
	c.supports(base.Add(versionProbeCacheMax*time.Millisecond), "newcomer.nel.pet", 1, 15, false, probe)

	if len(c.entries) != versionProbeCacheMax {
		t.Fatalf("entries = %d, want exactly cap %d after evicting one for the newcomer", len(c.entries), versionProbeCacheMax)
	}
	if _, ok := c.get("knot0.nel.pet"); ok {
		t.Error("oldest entry must be evicted to make room")
	}
	if _, ok := c.get("newcomer.nel.pet"); !ok {
		t.Error("newcomer must be retained")
	}
}
