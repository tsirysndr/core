package microvm

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"
	"time"

	"github.com/mdlayher/vsock"

	"tangled.org/core/spindle/agentproto"
	agentv1 "tangled.org/core/spindle/agentproto/gen"
)

const guestWorkflowUser = "spindle-workflow"

var errGuestTimedOut = errors.New("guest reported step timed out")

type agentHub struct {
	l       *slog.Logger
	ln      *vsock.Listener
	pending map[uint32]chan net.Conn
	mu      sync.Mutex
}

func newAgentHub(port uint32, l *slog.Logger) (*agentHub, error) {
	ln, err := vsock.Listen(port, nil)
	if err != nil {
		return nil, fmt.Errorf("listen for agent on vsock port %d: %w", port, err)
	}
	h := &agentHub{
		l:       l,
		ln:      ln,
		pending: make(map[uint32]chan net.Conn),
	}
	go h.acceptLoop()
	return h, nil
}

func (h *agentHub) expect(cid uint32) (<-chan net.Conn, func(), error) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if _, exists := h.pending[cid]; exists {
		return nil, nil, fmt.Errorf("already waiting for agent cid %d", cid)
	}
	ch := make(chan net.Conn, 1)
	h.pending[cid] = ch
	unregister := func() {
		h.mu.Lock()
		delete(h.pending, cid)
		h.mu.Unlock()
		close(ch)
		for conn := range ch {
			if conn != nil {
				_ = conn.Close()
			}
		}
	}
	return ch, unregister, nil
}

func (h *agentHub) acceptLoop() {
	for {
		conn, err := h.ln.Accept()
		if err != nil {
			h.l.Error("agent vsock accept failed", "error", err)
			return
		}

		addr, ok := conn.RemoteAddr().(*vsock.Addr)
		if !ok {
			h.l.Warn("agent connection has unexpected remote address", "remote", conn.RemoteAddr())
			_ = conn.Close()
			continue
		}

		h.mu.Lock()
		ch, ok := h.pending[addr.ContextID]
		if ok {
			delete(h.pending, addr.ContextID)
		}
		h.mu.Unlock()

		// todo: if / when we add agent recovery (reconnect) we should add a
		// boot-initialized session credential to prevent random connections...
		// checking cid here works to ensure for now since we dont attempt to
		// reconnect, so we block anything else thats not expected (and agent
		// runs first in the boot sequence always).
		if !ok {
			h.l.Warn("dropping agent connection for unknown cid", "cid", addr.ContextID)
			_ = conn.Close()
			continue
		}

		select {
		case ch <- conn:
		default:
			_ = conn.Close()
		}
	}
}

type AgentExec struct {
	*agentv1.ExecStart
	ID     string
	Stdout io.Writer
	Stderr io.Writer
}

type AgentSession struct {
	conn net.Conn
	enc  *agentproto.Encoder
	dec  *agentproto.Decoder
	l    *slog.Logger
	mu   sync.Mutex
}

func NewAgentSession(conn net.Conn, l *slog.Logger) *AgentSession {
	return &AgentSession{
		conn: conn,
		enc:  agentproto.NewEncoder(conn),
		dec:  agentproto.NewDecoder(conn),
		l:    l,
	}
}

func (s *AgentSession) Init(ctx context.Context, init *agentv1.Init) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	hello, err := s.decode(ctx)
	if err != nil {
		return fmt.Errorf("read agent hello: %w", err)
	}
	helloPayload := hello.Hello
	if helloPayload == nil {
		return fmt.Errorf("expected agent hello, got nil")
	}
	s.l.Info("agent connected", "protocol", helloPayload.ProtocolVersion, "version", helloPayload.AgentVersion, "boot", helloPayload.BootId, "nix", helloPayload.NixVersion)

	if err := s.enc.Encode(&agentproto.Message{
		Id:   "init",
		Init: init,
	}); err != nil {
		return fmt.Errorf("send agent init: %w", err)
	}
	return nil
}

func (s *AgentSession) Exec(ctx context.Context, exec AgentExec) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if exec.ID == "" {
		return 0, fmt.Errorf("empty ID passed to Exec")
	}

	if exec.ExecStart.TimeoutSeconds == 0 {
		exec.ExecStart.TimeoutSeconds = timeoutSeconds(ctx, guestTimeoutGrace)
	}

	if err := s.enc.Encode(&agentproto.Message{
		Id:        exec.ID,
		ExecStart: exec.ExecStart,
	}); err != nil {
		return 0, fmt.Errorf("send exec_start: %w", err)
	}

	for {
		msg, err := s.decode(ctx)
		if err != nil {
			return 0, err
		}
		if msg.BuiltPaths == nil && msg.Id != exec.ID {
			continue
		}

		if p := msg.ExecStdout; p != nil {
			_, _ = io.WriteString(exec.Stdout, p.Data)
		} else if p := msg.ExecStderr; p != nil {
			_, _ = io.WriteString(exec.Stderr, p.Data)
		} else if p := msg.BuiltPaths; p != nil {
			// s.l.Debug("guest built paths", "reason", p.Reason, "count", len(p.Paths))
		} else if p := msg.ExecExit; p != nil {
			if p.Error != "" {
				s.l.Warn("guest exec error", "id", msg.Id, "error", p.Error)
			}
			if p.TimedOut {
				return int(p.ExitCode), errGuestTimedOut
			}
			return int(p.ExitCode), nil
		}
	}
}

func (s *AgentSession) ActivateConfig(ctx context.Context, id string, req *agentv1.ActivateConfig) (*agentv1.ActivateConfigResult, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if id == "" {
		return nil, fmt.Errorf("empty ID passed to ActivateConfig")
	}
	if req.TimeoutSeconds == 0 {
		req.TimeoutSeconds = timeoutSeconds(ctx, guestTimeoutGrace)
	}
	if err := s.enc.Encode(&agentproto.Message{
		Id:             id,
		ActivateConfig: req,
	}); err != nil {
		return nil, fmt.Errorf("send activate_config: %w", err)
	}

	for {
		msg, err := s.decode(ctx)
		if err != nil {
			return nil, err
		}
		if msg.BuiltPaths == nil && msg.Id != id {
			continue
		}

		if p := msg.BuiltPaths; p != nil {
			// s.l.Debug("guest built paths", "reason", p.Reason, "count", len(p.Paths))
		} else if p := msg.ActivateConfigResult; p != nil {
			if p.Error != "" {
				return nil, fmt.Errorf("activate config failed: %s", p.Error)
			}
			if p.Toplevel == "" {
				return nil, fmt.Errorf("activate config returned empty toplevel")
			}
			return p, nil
		}
	}
}

func (s *AgentSession) Poweroff(ctx context.Context) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	id := "poweroff"
	if err := s.enc.Encode(&agentproto.Message{
		Id:       id,
		Poweroff: &agentv1.Poweroff{},
	}); err != nil {
		return fmt.Errorf("send poweroff: %w", err)
	}

	for {
		msg, err := s.decode(ctx)
		if err != nil {
			return err
		}
		if msg.Id != id {
			continue
		}
		p := msg.PoweroffResult
		if p == nil {
			continue
		}
		if p.Error != "" {
			return fmt.Errorf("guest poweroff failed: %s", p.Error)
		}
		return nil
	}
}

func (s *AgentSession) Drain(ctx context.Context) (uint32, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	drainID := "cache-drain"
	if err := s.enc.Encode(&agentproto.Message{
		Id: drainID,
		CacheDrain: &agentv1.CacheDrain{
			TimeoutSeconds: timeoutSeconds(ctx, 0),
		},
	}); err != nil {
		return 0, fmt.Errorf("send cache_drain: %w", err)
	}

	for {
		msg, err := s.decode(ctx)
		if err != nil {
			return 0, err
		}
		if msg.Id != drainID {
			continue
		}
		p := msg.CacheDrainResult
		if p == nil {
			continue
		}
		s.l.Info("cache drain complete", "uploaded", p.CacheUploaded, "failed", p.CacheFailed, "queued", p.CacheQueued, "active", p.CacheActive)
		if p.Error != "" {
			return 0, fmt.Errorf("cache drain failed: %s", p.Error)
		}
		if p.CacheFailed > 0 {
			return 0, fmt.Errorf("cache drain failed for %d paths", p.CacheFailed)
		}
		if p.CacheQueued > 0 || p.CacheActive > 0 {
			return 0, fmt.Errorf("cache drain incomplete: queued=%d active=%d", p.CacheQueued, p.CacheActive)
		}
		return p.CacheUploaded, nil
	}
}

func (s *AgentSession) decode(ctx context.Context) (*agentproto.Message, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	if deadline, ok := ctx.Deadline(); ok {
		_ = s.conn.SetReadDeadline(deadline)
	} else {
		_ = s.conn.SetReadDeadline(time.Time{})
	}

	// a blocked vsock read wont wake up just from the ctx being cancelled,
	// only a deadline will wake it up, so if the VM crashes mid-step the read would
	// hang until workflow timeout. so we will set a deadline in the past to cancel it.
	//
	// we set a deadline here instead of closing the connection, this is the long-lived
	// connection that everything reuses, so we only really want to interrupt it for this
	// current read. this also lands as a timeout error which the netErr.Timeout() check
	// below maps to ctx.Err() correctly
	stop := context.AfterFunc(ctx, func() {
		_ = s.conn.SetReadDeadline(time.Now())
	})
	defer stop()

	msg, err := s.dec.Decode()
	if err != nil {
		var netErr net.Error
		if errors.As(err, &netErr) && netErr.Timeout() && ctx.Err() != nil {
			return nil, ctx.Err()
		}
		return nil, fmt.Errorf("read agent message: %w", err)
	}
	return msg, nil
}

func (s *AgentSession) Close() error {
	if s == nil || s.conn == nil {
		return nil
	}
	return s.conn.Close()
}

// this pulls the deadline from the context and converts it to what the
// agentproto expects
func timeoutSeconds(ctx context.Context, lead time.Duration) uint32 {
	deadline, ok := ctx.Deadline()
	if !ok {
		return 0
	}
	seconds := int64((time.Until(deadline) - lead).Round(time.Second) / time.Second)
	if seconds < 1 {
		return 1
	}
	if seconds > int64(^uint32(0)) {
		return ^uint32(0)
	}
	return uint32(seconds)
}
