package knotacl

import (
	"context"
	"maps"
	"net/http"
	"slices"
	"sync"
	"time"

	"golang.org/x/sync/singleflight"
)

const (
	cacheTTL        = 15 * time.Second
	cacheMaxEntries = 4096
)

type lister interface {
	GetKnotMembers(ctx context.Context, host string) ([]string, error)
	GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error)
}

type cacheEntry struct {
	subjects []string
	storedAt time.Time
}

type cache struct {
	inner lister
	ttl   time.Duration
	now   func() time.Time

	mu      sync.Mutex
	entries map[string]cacheEntry
	group   singleflight.Group
}

func newCache(inner lister, ttl time.Duration, now func() time.Time) *cache {
	if now == nil {
		now = time.Now
	}
	return &cache{inner: inner, ttl: ttl, now: now, entries: map[string]cacheEntry{}}
}

func (c *cache) GetKnotMembers(ctx context.Context, host string) ([]string, error) {
	return c.fetch(ctx, memberCacheKey(host), func() ([]string, error) {
		return c.inner.GetKnotMembers(ctx, host)
	})
}

func (c *cache) GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error) {
	return c.fetch(ctx, collabCacheKey(host, repoDid), func() ([]string, error) {
		return c.inner.GetRepoCollaborators(ctx, host, repoDid)
	})
}

func memberCacheKey(host string) string { return "m\x00" + host }

func collabCacheKey(host, repoDid string) string { return "c\x00" + host + "\x00" + repoDid }

func (c *cache) InvalidateMembers(host string) {
	c.forget(memberCacheKey(host))
}

func (c *cache) InvalidateCollaborators(host, repoDid string) {
	c.forget(collabCacheKey(host, repoDid))
}

func (c *cache) forget(key string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	delete(c.entries, key)
}

func (c *cache) fetch(ctx context.Context, key string, load func() ([]string, error)) ([]string, error) {
	if memo := memoFrom(ctx); memo != nil {
		if v, ok := memo.get(key); ok {
			return slices.Clone(v), nil
		}
	}
	v, err := c.load(key, load)
	if err != nil {
		return nil, err
	}
	if memo := memoFrom(ctx); memo != nil {
		memo.put(key, v)
	}
	return slices.Clone(v), nil
}

func (c *cache) load(key string, load func() ([]string, error)) ([]string, error) {
	if v, ok := c.lookup(key); ok {
		return v, nil
	}
	v, err, _ := c.group.Do(key, func() (any, error) {
		if v, ok := c.lookup(key); ok {
			return v, nil
		}
		fresh, err := load()
		if err != nil {
			return nil, err
		}
		c.store(key, fresh)
		return fresh, nil
	})
	if err != nil {
		return nil, err
	}
	return v.([]string), nil
}

func (c *cache) lookup(key string) ([]string, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	e, ok := c.entries[key]
	if !ok || c.now().Sub(e.storedAt) >= c.ttl {
		return nil, false
	}
	return e.subjects, true
}

func (c *cache) store(key string, subjects []string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if _, exists := c.entries[key]; !exists && len(c.entries) >= cacheMaxEntries {
		maps.DeleteFunc(c.entries, func(_ string, e cacheEntry) bool {
			return c.now().Sub(e.storedAt) >= c.ttl
		})
		c.evictOldestLocked()
	}
	c.entries[key] = cacheEntry{subjects: subjects, storedAt: c.now()}
}

func (c *cache) evictOldestLocked() {
	oldestKey := ""
	var oldestAt time.Time
	for k, e := range c.entries {
		if oldestKey == "" || e.storedAt.Before(oldestAt) {
			oldestKey, oldestAt = k, e.storedAt
		}
	}
	if len(c.entries) >= cacheMaxEntries && oldestKey != "" {
		delete(c.entries, oldestKey)
	}
}

type requestMemo struct {
	mu      sync.Mutex
	entries map[string][]string
}

type memoCtxKey struct{}

func WithMemo(ctx context.Context) context.Context {
	return context.WithValue(ctx, memoCtxKey{}, &requestMemo{entries: map[string][]string{}})
}

func memoFrom(ctx context.Context) *requestMemo {
	memo, _ := ctx.Value(memoCtxKey{}).(*requestMemo)
	return memo
}

func (m *requestMemo) get(key string) ([]string, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	v, ok := m.entries[key]
	return v, ok
}

func (m *requestMemo) put(key string, subjects []string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.entries[key] = subjects
}

func MemoMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		next.ServeHTTP(w, r.WithContext(WithMemo(r.Context())))
	})
}
