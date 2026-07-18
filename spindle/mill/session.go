package mill

import (
	"context"
	"errors"
	"log/slog"
	"sync"
	"time"

	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

var errSessionClosed = errors.New("mill: executor session closed")

// one live websocket to an executor. many leases and async streams share
// it, so one reader goroutine demuxes by message type and correlates by
// lease ID. never hold locks across decodes
type millSession struct {
	nodeID         string
	epoch          string
	labels         []string
	enc            messageEncoder
	l              *slog.Logger
	closeTransport func() error

	// snapshot, disconnected and graceTimer are guarded by Mill.mu, the
	// fleet ranks across sessions under its own lock
	snapshot     *millv1.NodeSnapshot
	disconnected bool
	graceTimer   *time.Timer
	lastSeen     time.Time

	mu      sync.Mutex
	pending map[string]chan *millproto.Message // maps lease ID to response waiter

	closeOnce sync.Once
	closed    chan struct{}
}

type messageEncoder interface {
	Encode(*millproto.Message) error
}

func newSession(nodeID string, epoch string, labels []string, enc messageEncoder, l *slog.Logger) *millSession {
	return &millSession{
		nodeID:   nodeID,
		epoch:    epoch,
		labels:   labels,
		enc:      enc,
		l:        l,
		pending:  make(map[string]chan *millproto.Message),
		closed:   make(chan struct{}),
		lastSeen: time.Now(),
	}
}

// caller holds Mill.mu
func (s *millSession) live(grace time.Duration) bool {
	return !s.disconnected && time.Since(s.lastSeen) <= grace
}

func (s *millSession) send(msg *millproto.Message) error {
	return s.enc.Encode(msg)
}

func (s *millSession) close() {
	s.closeOnce.Do(func() {
		close(s.closed)
		if s.closeTransport != nil {
			_ = s.closeTransport()
		}
	})
}

// one-shot waiter for the next response on the lease, cancel unregisters it
func (s *millSession) await(leaseID string) (<-chan *millproto.Message, func()) {
	ch := make(chan *millproto.Message, 1)
	s.mu.Lock()
	s.pending[leaseID] = ch
	s.mu.Unlock()
	return ch, func() {
		s.mu.Lock()
		if s.pending[leaseID] == ch {
			delete(s.pending, leaseID)
		}
		s.mu.Unlock()
	}
}

func (s *millSession) deliver(leaseID string, msg *millproto.Message) {
	s.mu.Lock()
	ch := s.pending[leaseID]
	delete(s.pending, leaseID)
	s.mu.Unlock()
	if ch != nil {
		select {
		case ch <- msg:
		default:
		}
	}
}

// sends a message and waits for its response, respecting ctx and session closure
func (s *millSession) request(ctx context.Context, leaseID string, msg *millproto.Message) (*millproto.Message, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	ch, cancel := s.await(leaseID)
	defer cancel()

	if err := s.send(msg); err != nil {
		return nil, err
	}

	select {
	case resp := <-ch:
		return resp, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-s.closed:
		return nil, errSessionClosed
	}
}

// demuxes frames until the decoder errors (connection gone)
func (s *millSession) readLoop(m *Mill, dec *millproto.Decoder) error {
	for {
		msg, err := dec.Decode()
		if err != nil {
			return err
		}
		if err := s.dispatch(m, msg); err != nil {
			return err
		}
	}
}

func (s *millSession) dispatch(m *Mill, msg *millproto.Message) error {
	if !m.touchSession(s) {
		return errSessionClosed
	}
	switch {
	case msg.GetNodeSnapshot() != nil:
		return m.onSnapshot(s, msg.GetNodeSnapshot())
	case msg.GetReserveResult() != nil:
		s.deliver(msg.GetReserveResult().GetLeaseId(), msg)
	case msg.GetCommitted() != nil:
		s.deliver(msg.GetCommitted().GetLeaseId(), msg)
	case msg.GetEventBatch() != nil:
		return m.onEventBatch(s, msg.GetEventBatch())
	case msg.GetCancelAck() != nil:
		m.onCancelAck(s, msg.GetCancelAck())
	case msg.GetLiveLog() != nil:
		return m.onLiveLog(s, msg.GetLiveLog())
	default:
		s.l.Warn("session received unexpected message", "node", s.nodeID)
	}
	return nil
}
