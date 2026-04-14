package knotserver

import (
	"context"
	"encoding/json"
	"log/slog"
	"sync"
	"testing"

	jsmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/log"
)

type logRecord struct {
	Level slog.Level
	Msg   string
	Attrs map[string]any
}

type capturingHandler struct {
	mu      *sync.Mutex
	records *[]logRecord
	attrs   []slog.Attr
}

func newCapturingHandler() *capturingHandler {
	return &capturingHandler{
		mu:      &sync.Mutex{},
		records: &[]logRecord{},
	}
}

func (h *capturingHandler) Enabled(_ context.Context, _ slog.Level) bool { return true }

func (h *capturingHandler) Handle(_ context.Context, r slog.Record) error {
	rec := logRecord{Level: r.Level, Msg: r.Message, Attrs: map[string]any{}}
	for _, a := range h.attrs {
		rec.Attrs[a.Key] = a.Value.Any()
	}
	r.Attrs(func(a slog.Attr) bool {
		rec.Attrs[a.Key] = a.Value.Any()
		return true
	})
	h.mu.Lock()
	*h.records = append(*h.records, rec)
	h.mu.Unlock()
	return nil
}

func (h *capturingHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	merged := make([]slog.Attr, 0, len(h.attrs)+len(attrs))
	merged = append(merged, h.attrs...)
	merged = append(merged, attrs...)
	return &capturingHandler{mu: h.mu, records: h.records, attrs: merged}
}

func (h *capturingHandler) WithGroup(string) slog.Handler {
	panic("capturingHandler: WithGroup not supported")
}

func (h *capturingHandler) snapshot() []logRecord {
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make([]logRecord, len(*h.records))
	copy(out, *h.records)
	return out
}

func newProcessRepoFixture(t *testing.T) (*Knot, context.Context, *capturingHandler) {
	t.Helper()
	d := newTestKnotDB(t)
	cap := newCapturingHandler()
	l := slog.New(cap)
	ctx := log.IntoContext(context.Background(), l)

	c := &config.Config{
		Server: config.Server{Hostname: "knot.example"},
	}
	return &Knot{
		c:  c,
		db: d,
		l:  l,
	}, ctx, cap
}

func repoEvent(t *testing.T, authorDid, rkey, rev string, record tangled.Repo, op string) *jsmodels.Event {
	t.Helper()
	raw, err := json.Marshal(record)
	if err != nil {
		t.Fatalf("marshal record: %v", err)
	}
	return &jsmodels.Event{
		Did:  authorDid,
		Kind: jsmodels.EventKindCommit,
		Commit: &jsmodels.Commit{
			Operation:  op,
			Collection: tangled.RepoNSID,
			RKey:       rkey,
			Rev:        rev,
			Record:     raw,
		},
	}
}

func ptr(s string) *string { return &s }

func TestProcessRepo_CreateRegistersAlias(t *testing.T) {
	h, ctx, _ := newProcessRepoFixture(t)
	if err := h.db.StoreRepoKey("did:plc:repo1", []byte("k"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}

	ev := repoEvent(t, "did:plc:akshay", "bar", "3laaaaaaaaaab", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:repo1"),
	}, jsmodels.CommitOperationCreate)
	if err := h.processRepo(ctx, ev); err != nil {
		t.Fatalf("processRepo: %v", err)
	}

	_, current, err := h.db.CurrentRkey("did:plc:repo1")
	if err != nil {
		t.Fatalf("CurrentRkey: %v", err)
	}
	if current != "bar" {
		t.Errorf("current rkey = %q, want bar (highest rev alias)", current)
	}

	oldDid, err := h.db.GetRepoDid("did:plc:akshay", "foo")
	if err != nil || oldDid != "did:plc:repo1" {
		t.Errorf("old rkey foo should still resolve: got (%q, %v)", oldDid, err)
	}
}

func TestProcessRepo_DeleteRemovesAlias(t *testing.T) {
	h, ctx, _ := newProcessRepoFixture(t)
	if err := h.db.StoreRepoKey("did:plc:repo1", []byte("k"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := h.db.UpsertRepoAlias(db.RepoAlias{
		OwnerDid: "did:plc:akshay", Rkey: "bar", RepoDid: "did:plc:repo1", Rev: "3laaaaaaaaaab",
	}); err != nil {
		t.Fatalf("UpsertRepoAlias: %v", err)
	}

	ev := repoEvent(t, "did:plc:akshay", "bar", "3laaaaaaaaaac", tangled.Repo{}, jsmodels.CommitOperationDelete)
	if err := h.processRepo(ctx, ev); err != nil {
		t.Fatalf("processRepo: %v", err)
	}

	if _, err := h.db.GetRepoDid("did:plc:akshay", "bar"); err == nil {
		t.Errorf("bar alias should have been deleted")
	}

	_, current, _ := h.db.CurrentRkey("did:plc:repo1")
	if current != "foo" {
		t.Errorf("current rkey after delete = %q, want foo", current)
	}
}

func TestProcessRepo_MalformedJSONReturnsError(t *testing.T) {
	h, ctx, _ := newProcessRepoFixture(t)

	ev := &jsmodels.Event{
		Did:  "did:plc:akshay",
		Kind: jsmodels.EventKindCommit,
		Commit: &jsmodels.Commit{
			Operation:  jsmodels.CommitOperationCreate,
			Collection: tangled.RepoNSID,
			RKey:       "rkey1",
			Record:     []byte("{not valid json"),
		},
	}
	if err := h.processRepo(ctx, ev); err == nil {
		t.Fatalf("processRepo returned nil, want unmarshal error")
	}
}

func TestProcessRepo_NotOwnedRejected(t *testing.T) {
	h, ctx, _ := newProcessRepoFixture(t)
	if err := h.db.StoreRepoKey("did:plc:repo1", []byte("k"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}

	ev := repoEvent(t, "did:plc:mallory", "pwned", "3laaaaaaaaaab", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:repo1"),
	}, jsmodels.CommitOperationCreate)
	if err := h.processRepo(ctx, ev); err != nil {
		t.Fatalf("processRepo: %v", err)
	}

	_, current, _ := h.db.CurrentRkey("did:plc:repo1")
	if current != "foo" {
		t.Errorf("current rkey = %q, want foo (mallory's event must be rejected)", current)
	}
	if _, err := h.db.GetRepoDid("did:plc:mallory", "pwned"); err == nil {
		t.Errorf("mallory should not be able to register an alias on alice's repo")
	}
}

func TestProcessRepo_WrongKnotIgnored(t *testing.T) {
	h, ctx, _ := newProcessRepoFixture(t)
	if err := h.db.StoreRepoKey("did:plc:repo1", []byte("k"), "did:plc:akshay", "foo"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}

	ev := repoEvent(t, "did:plc:akshay", "bar", "3laaaaaaaaaab", tangled.Repo{
		Knot:    "other.example",
		RepoDid: ptr("did:plc:repo1"),
	}, jsmodels.CommitOperationCreate)
	if err := h.processRepo(ctx, ev); err != nil {
		t.Fatalf("processRepo: %v", err)
	}

	_, current, _ := h.db.CurrentRkey("did:plc:repo1")
	if current != "foo" {
		t.Errorf("current rkey = %q, want foo (foreign-knot event must be ignored)", current)
	}
}
