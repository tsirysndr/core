package knotcompat

import (
	"context"
	"fmt"
	"net/http"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/consts"
)

const (
	versionProbeTimeout  = 5 * time.Second
	versionProbeFresh    = 5 * time.Minute
	versionProbeTrust    = time.Hour
	versionProbeCacheMax = 4096
)

type versionProbeEntry struct {
	version  string
	probedAt time.Time
}

type versionProbeCache struct {
	mu      sync.Mutex
	entries map[string]versionProbeEntry
}

func (c *versionProbeCache) get(host string) (versionProbeEntry, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	e, ok := c.entries[host]
	return e, ok
}

func (c *versionProbeCache) put(host, version string, at time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if _, exists := c.entries[host]; !exists && len(c.entries) >= versionProbeCacheMax {
		c.evictLocked(at)
	}
	c.entries[host] = versionProbeEntry{version: version, probedAt: at}
}

func (c *versionProbeCache) evictLocked(now time.Time) {
	oldestHost := ""
	var oldestAt time.Time
	for h, e := range c.entries {
		if now.Sub(e.probedAt) >= versionProbeTrust {
			delete(c.entries, h)
			continue
		}
		if oldestHost == "" || e.probedAt.Before(oldestAt) {
			oldestHost, oldestAt = h, e.probedAt
		}
	}
	if len(c.entries) >= versionProbeCacheMax && oldestHost != "" {
		delete(c.entries, oldestHost)
	}
}

func (c *versionProbeCache) supports(now time.Time, host string, minMajor, minMinor int, failOpen bool, probe func() (string, bool)) bool {
	if e, ok := c.get(host); ok && now.Sub(e.probedAt) < versionProbeFresh {
		return atLeast(e.version, minMajor, minMinor)
	}
	if version, ok := probe(); ok {
		c.put(host, version, now)
		return atLeast(version, minMajor, minMinor)
	}
	if e, ok := c.get(host); ok && now.Sub(e.probedAt) < versionProbeTrust {
		return atLeast(e.version, minMajor, minMinor)
	}
	return failOpen
}

var probeCache = &versionProbeCache{entries: map[string]versionProbeEntry{}}

type NativeLatch interface {
	IsNative(host string) bool
	MarkNative(host string)
}

type latchBox struct{ l NativeLatch }

type nativeGate struct {
	memo    sync.Map
	negMemo sync.Map
	latch   atomic.Pointer[latchBox]
	useOnce sync.Once
	now     func() time.Time
}

func (g *nativeGate) clock() time.Time {
	if g.now != nil {
		return g.now()
	}
	return time.Now()
}

func (g *nativeGate) use(l NativeLatch) {
	g.useOnce.Do(func() {
		g.latch.Store(&latchBox{l: l})
	})
}

func (g *nativeGate) currentLatch() NativeLatch {
	if b := g.latch.Load(); b != nil {
		return b.l
	}
	return nil
}

func (g *nativeGate) isNative(host string, probe func() bool) bool {
	if _, ok := g.memo.Load(host); ok {
		return true
	}
	if until, ok := g.negMemo.Load(host); ok {
		if g.clock().Before(until.(time.Time)) {
			return false
		}
		g.negMemo.Delete(host)
	}
	if l := g.currentLatch(); l != nil && l.IsNative(host) {
		g.memo.Store(host, struct{}{})
		return true
	}
	if !probe() {
		g.negMemo.Store(host, g.clock().Add(versionProbeFresh))
		return false
	}
	g.memo.Store(host, struct{}{})
	if l := g.currentLatch(); l != nil {
		l.MarkNative(host)
	}
	return true
}

var nativeProbeGate = &nativeGate{}

func UseNativeLatch(l NativeLatch) {
	nativeProbeGate.use(l)
}

func KnotSupports114(ctx context.Context, host string, dev bool) bool {
	return knotSupportsVersion(ctx, host, dev, 1, 14, true)
}

func KnotHasCapability(ctx context.Context, host string, dev bool, capability consts.Capability) bool {
	return nativeProbeGate.isNative(host, func() bool {
		return knotDeclares(ctx, host, dev, capability)
	})
}

func knotDeclares(ctx context.Context, host string, dev bool, capability consts.Capability) bool {
	scheme := "https"
	if dev {
		scheme = "http"
	}
	client := &indigoxrpc.Client{
		Host:   fmt.Sprintf("%s://%s", scheme, host),
		Client: &http.Client{Timeout: versionProbeTimeout},
	}

	ctx, cancel := context.WithTimeout(ctx, versionProbeTimeout)
	defer cancel()

	resp, err := tangled.KnotVersion(ctx, client)
	if err != nil || resp == nil {
		return false
	}
	return slices.Contains(resp.Capabilities, string(capability))
}

func knotSupportsVersion(ctx context.Context, host string, dev bool, minMajor, minMinor int, failOpen bool) bool {
	return probeCache.supports(time.Now(), host, minMajor, minMinor, failOpen, func() (string, bool) {
		return probeVersion(ctx, host, dev)
	})
}

func probeVersion(ctx context.Context, host string, dev bool) (string, bool) {
	scheme := "https"
	if dev {
		scheme = "http"
	}
	client := &indigoxrpc.Client{
		Host:   fmt.Sprintf("%s://%s", scheme, host),
		Client: &http.Client{Timeout: versionProbeTimeout},
	}

	ctx, cancel := context.WithTimeout(ctx, versionProbeTimeout)
	defer cancel()

	resp, err := tangled.KnotVersion(ctx, client)
	if err != nil || resp == nil {
		return "", false
	}
	return resp.Version, true
}

func atLeast(v string, minMajor, minMinor int) bool {
	v = strings.TrimSpace(v)
	v = strings.TrimPrefix(v, "v")
	if strings.HasPrefix(v, "(devel)") {
		return true
	}
	if v == "" {
		return false
	}
	parts := strings.SplitN(v, ".", 3)
	if len(parts) < 2 {
		return false
	}
	major, err := strconv.Atoi(parts[0])
	if err != nil {
		return false
	}
	minorRaw := strings.SplitN(parts[1], "-", 2)[0]
	minor, err := strconv.Atoi(minorRaw)
	if err != nil {
		return false
	}
	return major > minMajor || (major == minMajor && minor >= minMinor)
}
