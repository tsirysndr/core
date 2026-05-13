package oauth

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/bluesky-social/indigo/atproto/auth/oauth"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/hashicorp/golang-lru/v2/expirable"
)

func discardLogger(t *testing.T) *slog.Logger {
	t.Helper()
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

type stubStore struct {
	mu              sync.Mutex
	data            map[string]oauth.ClientSessionData
	getSessionCalls atomic.Int32
	deleteCalls     atomic.Int32
}

func (s *stubStore) key(did syntax.DID, sessId string) string {
	return string(did) + ":" + sessId
}

func (s *stubStore) GetSession(_ context.Context, did syntax.DID, sessId string) (*oauth.ClientSessionData, error) {
	s.getSessionCalls.Add(1)
	s.mu.Lock()
	defer s.mu.Unlock()
	v, ok := s.data[s.key(did, sessId)]
	if !ok {
		return nil, errors.New("no such session")
	}
	clone := v
	return &clone, nil
}

func (s *stubStore) SaveSession(_ context.Context, sess oauth.ClientSessionData) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.data[s.key(sess.AccountDID, sess.SessionID)] = sess
	return nil
}

func (s *stubStore) DeleteSession(_ context.Context, did syntax.DID, sessId string) error {
	s.deleteCalls.Add(1)
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.data, s.key(did, sessId))
	return nil
}

func (s *stubStore) GetAuthRequestInfo(context.Context, string) (*oauth.AuthRequestData, error) {
	return nil, errors.New("not used")
}
func (s *stubStore) SaveAuthRequestInfo(context.Context, oauth.AuthRequestData) error {
	return nil
}
func (s *stubStore) DeleteAuthRequestInfo(context.Context, string) error { return nil }

func newTestOAuth(t *testing.T) (*OAuth, *stubStore) {
	t.Helper()
	priv, err := atcrypto.GeneratePrivateKeyP256()
	if err != nil {
		t.Fatalf("generate key: %v", err)
	}
	store := &stubStore{data: map[string]oauth.ClientSessionData{}}
	store.data[store.key("did:plc:boltless", "sess1")] = oauth.ClientSessionData{
		AccountDID:              "did:plc:boltless",
		SessionID:               "sess1",
		HostURL:                 "https://pds.example",
		AuthServerURL:           "https://pds.example",
		AuthServerTokenEndpoint: "https://pds.example/oauth/token",
		DPoPPrivateKeyMultibase: priv.Multibase(),
	}

	cfg := oauth.NewLocalhostConfig("http://127.0.0.1/cb", []string{"atproto"})
	app := oauth.NewClientApp(&cfg, store)
	o := &OAuth{
		ClientApp:    app,
		Logger:       discardLogger(t),
		sessionCache: expirable.NewLRU[string, *oauth.ClientSession](sessionCacheSize, nil, sessionCacheTTL),
	}
	return o, store
}

func TestResumeSessionSingleflightDedupes(t *testing.T) {
	o, store := newTestOAuth(t)

	const n = 32
	var wg sync.WaitGroup
	results := make([]*oauth.ClientSession, n)
	errs := make([]error, n)
	wg.Add(n)
	for i := range n {
		go func() {
			defer wg.Done()
			sess, err := o.resumeSession(context.Background(), "did:plc:boltless", "sess1")
			results[i] = sess
			errs[i] = err
		}()
	}
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Fatalf("goroutine %d: %v", i, err)
		}
	}
	first := results[0]
	if first == nil {
		t.Fatal("first session is nil")
	}
	for i, s := range results {
		if s != first {
			t.Fatalf("goroutine %d got different *ClientSession (%p vs %p)", i, s, first)
		}
	}
	calls := store.getSessionCalls.Load()
	if calls > 1 {
		t.Fatalf("GetSession called %d times, want 1", calls)
	}
}

func TestResumeSessionReuseAfterCache(t *testing.T) {
	o, store := newTestOAuth(t)

	a, err := o.resumeSession(context.Background(), "did:plc:boltless", "sess1")
	if err != nil {
		t.Fatalf("first: %v", err)
	}
	b, err := o.resumeSession(context.Background(), "did:plc:boltless", "sess1")
	if err != nil {
		t.Fatalf("second: %v", err)
	}
	if a != b {
		t.Fatalf("expected same pointer across cache hit")
	}
	if got := store.getSessionCalls.Load(); got != 1 {
		t.Fatalf("GetSession called %d times, want 1", got)
	}
}

func TestHandlePermanentAuthErrEvictsAndLogsOut(t *testing.T) {
	o, store := newTestOAuth(t)

	if _, err := o.resumeSession(context.Background(), "did:plc:boltless", "sess1"); err != nil {
		t.Fatalf("seed: %v", err)
	}
	if _, ok := o.sessionCache.Get(sessionCacheKey("did:plc:boltless", "sess1")); !ok {
		t.Fatal("cache missing after resume")
	}

	handled := o.HandlePermanentAuthErr(
		context.Background(), "did:plc:boltless", "sess1",
		errors.New("auth server request failed (HTTP 400): invalid_grant"),
	)
	if !handled {
		t.Fatal("HandlePermanentAuthErr returned false")
	}
	if _, ok := o.sessionCache.Get(sessionCacheKey("did:plc:boltless", "sess1")); ok {
		t.Fatal("cache still holds entry after HandlePermanentAuthErr")
	}
	if got := store.deleteCalls.Load(); got != 1 {
		t.Fatalf("store.DeleteSession called %d times, want 1", got)
	}
	if _, ok := store.data[store.key("did:plc:boltless", "sess1")]; ok {
		t.Fatal("store still holds session after Logout")
	}
}

func TestHandlePermanentAuthErrIgnoresTransient(t *testing.T) {
	o, store := newTestOAuth(t)
	if _, err := o.resumeSession(context.Background(), "did:plc:boltless", "sess1"); err != nil {
		t.Fatalf("seed: %v", err)
	}
	handled := o.HandlePermanentAuthErr(
		context.Background(), "did:plc:boltless", "sess1",
		errors.New("token refresh failed (HTTP 429): rate_limited"),
	)
	if handled {
		t.Fatal("HandlePermanentAuthErr matched a transient error")
	}
	if _, ok := o.sessionCache.Get(sessionCacheKey("did:plc:boltless", "sess1")); !ok {
		t.Fatal("transient error evicted cache")
	}
	if got := store.deleteCalls.Load(); got != 0 {
		t.Fatalf("store.DeleteSession called %d times, want 0", got)
	}
}
