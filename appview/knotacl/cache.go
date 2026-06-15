package knotacl

import (
	"context"
	"net/http"
	"sync"
)

type lister interface {
	GetKnotMembers(ctx context.Context, host string) ([]string, error)
	GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error)
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
