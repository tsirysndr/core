package mill

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"tangled.org/core/notifier"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"

	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

func discardLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func nopEncoder() scriptedEncoder {
	return scriptedEncoder(func(*millproto.Message) error { return nil })
}

func TestHashToken(t *testing.T) {
	const raw = "super-secret-executor-token"

	if HashToken(raw) != HashToken(raw) {
		t.Fatal("HashToken is not deterministic; the same token would stop authenticating")
	}
	if HashToken("token-a") == HashToken("token-b") {
		t.Fatal("HashToken collided two distinct tokens")
	}
	if HashToken(raw) == raw {
		t.Fatal("HashToken returned the raw token; a hash leak would expose a usable credential")
	}
}

func TestGenerateTokenDistinct(t *testing.T) {
	const n = 100
	seen := make(map[string]struct{}, n)
	for i := range n {
		tok, err := GenerateToken()
		if err != nil {
			t.Fatalf("GenerateToken: %v", err)
		}
		if tok == "" {
			t.Fatalf("GenerateToken returned an empty token on call %d", i)
		}
		if _, dup := seen[tok]; dup {
			t.Fatalf("GenerateToken repeated a token after %d calls: %q", i, tok)
		}
		seen[tok] = struct{}{}
	}
}

func TestAttachSessionRejectsSecondLiveSession(t *testing.T) {
	l := discardLogger()
	m := New(l, Config{ReconnectGrace: time.Minute})

	sessionOf := func(node string) *millSession {
		m.mu.Lock()
		defer m.mu.Unlock()
		return m.sessions[node]
	}

	sess1 := newSession("node-1", "inc-1", nil, nopEncoder(), l)
	if _, ok := m.attachSession(sess1); !ok {
		t.Fatal("first attach of a node was rejected; want accept")
	}
	if sessionOf("node-1") != sess1 {
		t.Fatal("first session was not registered as the live session")
	}

	sess2 := newSession("node-1", "inc-2", nil, nopEncoder(), l)
	if _, ok := m.attachSession(sess2); ok {
		t.Fatal("second live attach for an already-live node was accepted; a valid token hijacked the executor")
	}
	if sessionOf("node-1") != sess1 {
		t.Fatal("rejected newcomer evicted the incumbent session")
	}

	m.detachSession(sess1)
	sess3 := newSession("node-1", "inc-3", nil, nopEncoder(), l)
	if _, ok := m.attachSession(sess3); !ok {
		t.Fatal("attach during the incumbent's reconnect grace was rejected; want adopt")
	}
	if sessionOf("node-1") != sess3 {
		t.Fatal("adopted session was not installed as the live session")
	}
}

func TestOnAttemptResultIgnoresForeignLease(t *testing.T) {
	ctx := context.Background()
	l := discardLogger()
	bdb, err := db.Make(ctx, filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()

	m := New(l, Config{ReconnectGrace: time.Minute})
	m.Attach(bdb, &n)

	foreign := newLease("lease-foreign", "node-a", "inc-a", "dummy")
	m.mu.Lock()
	m.leases[foreign.id] = foreign
	m.mu.Unlock()

	sessB := newSession("node-b", "inc-b", nil, nopEncoder(), l)
	m.attachSession(sessB)
	_ = m.onEventBatch(sessB, &millv1.EventBatch{
		Epoch: sessB.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: foreign.id,
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_SUCCESS,
					},
				},
			},
		},
	})

	if _, ok := pollTerminal(foreign); ok {
		t.Fatal("attempt-result on a foreign lease delivered a terminal; an executor forged another node's job result")
	}
	if foreign.getState() == leaseDone {
		t.Fatal("attempt-result on a foreign lease sealed the lease")
	}
}

func TestOnAttemptResultIgnoresAbsentLease(t *testing.T) {
	ctx := context.Background()
	l := discardLogger()
	bdb, err := db.Make(ctx, filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()

	m := New(l, Config{ReconnectGrace: time.Minute})
	m.Attach(bdb, &n)

	// the reporting node owns this bystander lease, proving an absent-lease
	// stream does not spill onto another lease
	bystander := newLease("lease-bystander", "node-b", "inc-b", "dummy")
	m.mu.Lock()
	m.leases[bystander.id] = bystander
	m.mu.Unlock()

	sessB := newSession("node-b", "inc-b", nil, nopEncoder(), l)
	m.attachSession(sessB)

	_ = m.onEventBatch(sessB, &millv1.EventBatch{
		Epoch: sessB.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: "lease-nonexistent",
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_SUCCESS,
					},
				},
			},
		},
	})

	if _, ok := pollTerminal(bystander); ok {
		t.Fatal("attempt-result for an absent lease delivered a terminal to a bystander lease")
	}
	if bystander.getState() == leaseDone {
		t.Fatal("attempt-result for an absent lease sealed a bystander lease")
	}
}

// owned-lease path proves ignore tests above are not passing merely because
// delivery is broken. correctly owned terminal is delivered
func TestOnAttemptResultDeliversOwnedLease(t *testing.T) {
	ctx := context.Background()
	l := discardLogger()
	bdb, err := db.Make(ctx, filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()

	m := New(l, Config{ReconnectGrace: time.Minute})
	m.Attach(bdb, &n)

	owned := newLease("lease-owned", "node-b", "inc-b", "dummy")
	m.mu.Lock()
	m.leases[owned.id] = owned
	m.mu.Unlock()

	sessB := newSession("node-b", "inc-b", nil, nopEncoder(), l)
	m.attachSession(sessB)
	_ = m.onEventBatch(sessB, &millv1.EventBatch{
		Epoch: sessB.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: owned.id,
				Payload: &millv1.Event_AttemptResult{
					AttemptResult: &millv1.AttemptResult{
						Status: millv1.TerminalStatus_SUCCESS,
					},
				},
			},
		},
	})

	res, ok := pollTerminal(owned)
	if !ok {
		t.Fatal("attempt-result on an owned lease was not delivered")
	}
	if got := res.GetStatus(); got != millv1.TerminalStatus_SUCCESS {
		t.Fatalf("delivered terminal status = %v, want %v", got, millv1.TerminalStatus_SUCCESS)
	}
	if owned.getState() != leaseDone {
		t.Fatal("owned lease was not sealed after its terminal was delivered")
	}
}

func TestOnStatusEventOwnership(t *testing.T) {
	ctx := context.Background()
	l := discardLogger()

	bdb, err := db.Make(ctx, filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()

	m := New(l, Config{ReconnectGrace: time.Minute})
	m.Attach(bdb, &n)

	foreign := newLease("lease-foreign", "node-x", "inc-x", "dummy")
	foreign.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "foreign"}, Name: "build"}
	owned := newLease("lease-owned", "node-z", "inc-z", "dummy")
	owned.wid = models.WorkflowId{PipelineId: models.PipelineId{Knot: "k", Rkey: "owned"}, Name: "build"}
	m.mu.Lock()
	m.leases[foreign.id] = foreign
	m.leases[owned.id] = owned
	m.mu.Unlock()

	sessY := newSession("node-y", "inc-y", nil, nopEncoder(), l)
	m.attachSession(sessY)
	sessZ := newSession("node-z", "inc-z", nil, nopEncoder(), l)
	m.attachSession(sessZ)

	_ = m.onEventBatch(sessY, &millv1.EventBatch{
		Epoch: sessY.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: foreign.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{
						Status: millv1.NonterminalStatus_RUNNING,
					},
				},
			},
		},
	})
	if _, err := bdb.GetStatus(foreign.wid); err == nil {
		t.Fatal("status stream for a foreign lease authored a status row; an executor forged another pipeline's status")
	}

	_ = m.onEventBatch(sessZ, &millv1.EventBatch{
		Epoch: sessZ.epoch,
		Events: []*millv1.Event{
			{
				Seqno:   1,
				LeaseId: owned.id,
				Payload: &millv1.Event_StatusEvent{
					StatusEvent: &millv1.StatusEvent{
						Status: millv1.NonterminalStatus_RUNNING,
					},
				},
			},
		},
	})
	st, err := bdb.GetStatus(owned.wid)
	if err != nil {
		t.Fatalf("owned status stream did not author a status row: %v", err)
	}
	if st.Status != "running" {
		t.Fatalf("owned status = %q, want %q", st.Status, "running")
	}
}

func setupTestServer(t *testing.T, authorizedLabels []string) (*Mill, *db.DB, *httptest.Server, string) {
	ctx := context.Background()
	bdb, err := db.Make(ctx, filepath.Join(t.TempDir(), "mill.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { bdb.Close() })
	n := notifier.New()

	m := New(discardLogger(), Config{
		ReconnectGrace: time.Minute,
	})
	m.Attach(bdb, &n)

	const secret = "test-secret"
	if err := bdb.AddExecutorToken("dev-node", HashToken(secret), nil, authorizedLabels); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}

	server := httptest.NewServer(http.HandlerFunc(m.HandleExecutorConn))
	t.Cleanup(server.Close)

	return m, bdb, server, secret
}

func TestAuthLabelEscalation(t *testing.T) {
	_, _, server, secret := setupTestServer(t, []string{"linux", "amd64"})

	wsUrl := "ws" + strings.TrimPrefix(server.URL, "http")

	{
		header := http.Header{}
		header.Set("Authorization", "Bearer bad-token")
		_, resp, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err == nil {
			t.Fatal("expected connection with invalid token to fail")
		}
		if resp != nil && resp.StatusCode != http.StatusUnauthorized {
			t.Fatalf("expected 401 Unauthorized, got %d", resp.StatusCode)
		}
	}

	{
		header := http.Header{}
		header.Set("Authorization", "Bearer "+secret)
		conn, _, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err != nil {
			t.Fatalf("dial failed: %v", err)
		}
		defer conn.Close()

		stream := millproto.NewWSStream(conn)
		enc := millproto.NewEncoder(stream)
		dec := millproto.NewDecoder(stream)

		hello := &millproto.Message{Hello: &millv1.Hello{
			ProtocolVersion: millproto.ProtocolVersion,
			Arch:            "amd64",
			Labels:          []string{"linux", "gpu"},
			Epoch:           "inc-1",
		}}
		if err := enc.Encode(hello); err != nil {
			t.Fatalf("encode hello: %v", err)
		}

		_, err = dec.Decode()
		if err == nil {
			t.Fatal("expected server to close connection for unauthorized label, but got a message")
		}
	}

	{
		header := http.Header{}
		header.Set("Authorization", "Bearer "+secret)
		conn, _, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err != nil {
			t.Fatalf("dial failed: %v", err)
		}
		defer conn.Close()

		stream := millproto.NewWSStream(conn)
		enc := millproto.NewEncoder(stream)
		dec := millproto.NewDecoder(stream)

		hello := &millproto.Message{Hello: &millv1.Hello{
			ProtocolVersion: millproto.ProtocolVersion,
			Arch:            "amd64",
			Labels:          []string{"linux"},
			Epoch:           "inc-1",
		}}
		if err := enc.Encode(hello); err != nil {
			t.Fatalf("encode hello: %v", err)
		}

		msg, err := dec.Decode()
		if err != nil {
			t.Fatalf("expected resume message, got error: %v", err)
		}
		res := msg.GetResume()
		if res == nil {
			t.Fatal("expected Resume message, got nil")
		}
		if res.GetEpoch() != "inc-1" {
			t.Fatalf("expected epoch inc-1, got %q", res.GetEpoch())
		}
	}
}

func TestHandshakeTimeoutAndConcurrency(t *testing.T) {
	_, _, server, secret := setupTestServer(t, []string{"linux"})
	wsUrl := "ws" + strings.TrimPrefix(server.URL, "http")

	// executor that never sends Hello is dropped after the 5s pre-hello deadline
	{
		header := http.Header{}
		header.Set("Authorization", "Bearer "+secret)
		conn, _, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err != nil {
			t.Fatalf("dial failed: %v", err)
		}
		defer conn.Close()

		time.Sleep(6 * time.Second)

		stream := millproto.NewWSStream(conn)
		enc := millproto.NewEncoder(stream)
		hello := &millproto.Message{Hello: &millv1.Hello{
			ProtocolVersion: millproto.ProtocolVersion,
			Arch:            "amd64",
			Labels:          []string{"linux"},
			Epoch:           "inc-1",
		}}
		err = enc.Encode(hello)
		dec := millproto.NewDecoder(stream)
		_, readErr := dec.Decode()
		if readErr == nil {
			t.Fatal("expected server to have closed connection due to handshake timeout")
		}
	}

	// second live session for one identity is rejected with 409 before the ws upgrade
	{
		header := http.Header{}
		header.Set("Authorization", "Bearer "+secret)

		conn1, _, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err != nil {
			t.Fatalf("dial 1 failed: %v", err)
		}
		defer conn1.Close()

		stream1 := millproto.NewWSStream(conn1)
		enc1 := millproto.NewEncoder(stream1)
		dec1 := millproto.NewDecoder(stream1)
		hello1 := &millproto.Message{Hello: &millv1.Hello{
			ProtocolVersion: millproto.ProtocolVersion,
			Arch:            "amd64",
			Labels:          []string{"linux"},
			Epoch:           "inc-1",
		}}
		if err := enc1.Encode(hello1); err != nil {
			t.Fatalf("encode hello 1: %v", err)
		}
		_, err = dec1.Decode()
		if err != nil {
			t.Fatalf("first connection handshake failed: %v", err)
		}

		_, resp, err := websocket.DefaultDialer.Dial(wsUrl, header)
		if err == nil {
			t.Fatal("expected second connection for same live identity to be rejected")
		}
		if resp != nil && resp.StatusCode != http.StatusConflict {
			t.Fatalf("expected 409 Conflict for duplicate session, got %d", resp.StatusCode)
		}
	}
}
