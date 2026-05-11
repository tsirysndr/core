package xrpc

import (
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type InflightEntry struct {
	ID         uint64    `json:"id"`
	Method     string    `json:"method"`
	Path       string    `json:"path"`
	RawQuery   string    `json:"raw_query,omitempty"`
	URL        string    `json:"url"`
	Repo       string    `json:"repo,omitempty"`
	RemoteAddr string    `json:"remote_addr,omitempty"`
	StartedAt  time.Time `json:"started_at"`
	DurationMs int64     `json:"duration_ms"`
}

type inflightTracker struct {
	mu     sync.Mutex
	nextID atomic.Uint64
	active map[uint64]*InflightEntry
}

func newInflightTracker() *inflightTracker {
	return &inflightTracker{active: make(map[uint64]*InflightEntry)}
}

func (t *inflightTracker) add(r *http.Request) uint64 {
	id := t.nextID.Add(1)
	q := r.URL.Query()
	entry := &InflightEntry{
		ID:         id,
		Method:     r.Method,
		Path:       r.URL.Path,
		RawQuery:   r.URL.RawQuery,
		URL:        r.URL.RequestURI(),
		Repo:       q.Get("repo"),
		RemoteAddr: clientAddr(r),
		StartedAt:  time.Now(),
	}
	t.mu.Lock()
	t.active[id] = entry
	t.mu.Unlock()
	return id
}

func (t *inflightTracker) remove(id uint64) {
	t.mu.Lock()
	delete(t.active, id)
	t.mu.Unlock()
}

func (t *inflightTracker) snapshot() []InflightEntry {
	t.mu.Lock()
	defer t.mu.Unlock()
	out := make([]InflightEntry, 0, len(t.active))
	now := time.Now()
	for _, e := range t.active {
		cp := *e
		cp.DurationMs = now.Sub(e.StartedAt).Milliseconds()
		out = append(out, cp)
	}
	return out
}

func clientAddr(r *http.Request) string {
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		if i := strings.IndexByte(xff, ','); i >= 0 {
			return strings.TrimSpace(xff[:i])
		}
		return strings.TrimSpace(xff)
	}
	return r.RemoteAddr
}

func (t *inflightTracker) middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := t.add(r)
		defer t.remove(id)
		next.ServeHTTP(w, r)
	})
}

func (x *Xrpc) Inflight() []InflightEntry {
	return x.inflight.snapshot()
}
