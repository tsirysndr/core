//go:build linux

package microvm

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"log/slog"
	"net"
	"regexp"
	"strings"
	"sync"
	"time"

	"tangled.org/core/spindle/agentproto"
	agentv1 "tangled.org/core/spindle/agentproto/gen"
	"tangled.org/core/spindle/models"
)

const debugAcceptTimeout = 15 * time.Second

type debugTarget struct {
	cid        uint32
	agent      *AgentSession
	knot       string
	repoDid    string
	maxAliveAt time.Time
	connected  chan struct{} // closed when the user first ssh's in, ending the grace window
	released   chan struct{} // closed when the user exits the debug shell, to tear down early
}

var debugHandleSafe = regexp.MustCompile(`[^a-zA-Z0-9_.-]`)

func newDebugHandle(wid models.WorkflowId) string {
	h := fnv.New32a()
	_, _ = io.WriteString(h, wid.String())
	token := hex.EncodeToString(h.Sum(nil))
	name := strings.Trim(debugHandleSafe.ReplaceAllString(wid.Name, "-"), "-")
	if name == "" {
		return token
	}
	return name + "-" + token
}

func (e *Engine) registerDebugTarget(wid models.WorkflowId, t debugTarget) {
	e.debugMu.Lock()
	defer e.debugMu.Unlock()
	e.debug[newDebugHandle(wid)] = t
}

func (e *Engine) unregisterDebugTarget(wid models.WorkflowId) {
	e.debugMu.Lock()
	defer e.debugMu.Unlock()
	delete(e.debug, newDebugHandle(wid))
}

// ends the grace window so we hold the VM until the shell exits instead.
func (e *Engine) markDebugConnected(handle string) {
	e.debugMu.Lock()
	defer e.debugMu.Unlock()
	if t, ok := e.debug[handle]; ok {
		select {
		case <-t.connected: // already signalled
		default:
			close(t.connected)
		}
	}
}

func (e *Engine) releaseDebugTarget(handle string) {
	e.debugMu.Lock()
	defer e.debugMu.Unlock()
	t, ok := e.debug[handle]
	if !ok {
		return
	}
	delete(e.debug, handle)
	close(t.released)
}

func (e *Engine) lookupDebugTarget(handle string) (debugTarget, bool) {
	e.debugMu.Lock()
	defer e.debugMu.Unlock()
	t, ok := e.debug[handle]
	return t, ok
}

func (e *Engine) RepoForJob(jobID string) (knot, repoDid string, ok bool) {
	t, found := e.lookupDebugTarget(jobID)
	if !found || t.knot == "" || t.repoDid == "" {
		return "", "", false
	}
	return t.knot, t.repoDid, true
}

func (e *Engine) OpenDebugSession(ctx context.Context, jobID, term string, rows, cols int) (*DebugSession, error) {
	target, ok := e.lookupDebugTarget(jobID)
	if !ok {
		return nil, fmt.Errorf("no live microVM for job %q", jobID)
	}

	ln, port, err := listenRandomVsockPort(ctx)
	if err != nil {
		return nil, fmt.Errorf("listen for debug shell: %w", err)
	}
	filtered := &cidFilteredVsockListener{Listener: ln, cid: target.cid, logger: e.l}

	if err := target.agent.OpenDebugShell(&agentv1.OpenDebugShell{
		VsockPort: port,
		Term:      term,
		Rows:      clampDim(rows),
		Cols:      clampDim(cols),
	}); err != nil {
		_ = ln.Close()
		return nil, fmt.Errorf("ask guest to open debug shell: %w", err)
	}

	conn, err := acceptWithTimeout(ctx, filtered, debugAcceptTimeout)
	if err != nil {
		_ = ln.Close()
		return nil, fmt.Errorf("accept debug shell connection: %w", err)
	}

	e.markDebugConnected(jobID)

	return newDebugSession(conn, ln, e.l), nil
}

func (e *Engine) writeDebugHint(wid models.WorkflowId, idx int, wfLogger models.WorkflowLogger) {
	if wfLogger == nil {
		wfLogger = models.NullLogger{}
	}
	step := Step{name: "Debug shell", kind: models.StepKindSystem}

	wfLogger.ControlWriter(idx, step, models.StepStatusStart).Write([]byte{0})
	defer wfLogger.ControlWriter(idx, step, models.StepStatusEnd).Write([]byte{0})

	ssh := e.cfg.MicroVMPipelines.DebugSSH
	grace := ssh.GracePeriod
	cmd := debugSSHCommand(ssh.ListenAddr, e.cfg.Server.Hostname, ssh.Host, ssh.JumpHost, newDebugHandle(wid))
	out := wfLogger.DataWriter(idx, "stdout")
	fmt.Fprintf(out, "Workflow failed, connect within %s to debug until shell exit or workflow timeout:\n", grace)
	fmt.Fprintf(out, "    %s\n", cmd)
}

func (e *Engine) maybeRetainForDebug(ctx context.Context, wid models.WorkflowId) {
	handle := newDebugHandle(wid)
	target, ok := e.lookupDebugTarget(handle)
	if !ok {
		return
	}

	grace := e.cfg.MicroVMPipelines.DebugSSH.GracePeriod
	e.l.Info("retaining failed microVM for debug", "workflow", wid, "grace", grace.String())

	maxAlive := time.NewTimer(time.Until(target.maxAliveAt))
	defer maxAlive.Stop()
	graceTimer := time.NewTimer(grace)
	defer graceTimer.Stop()

	// wait for the user to ssh in within the grace window
	select {
	case <-ctx.Done():
		return
	case <-maxAlive.C:
		e.l.Info("debug retention hit max VM lifetime; tearing down microVM", "workflow", wid)
		return
	case <-graceTimer.C:
		e.l.Info("nobody ssh'd in within grace; tearing down microVM", "workflow", wid)
		return
	case <-target.connected:
		e.l.Info("debug shell connected; holding microVM until exit", "workflow", wid)
	}

	// connected: hold the VM until the user exits or it hits its max lifetime
	select {
	case <-ctx.Done():
	case <-maxAlive.C:
		e.l.Info("debug session hit max VM lifetime; tearing down microVM", "workflow", wid)
	case <-target.released:
		e.l.Info("debug shell exited; tearing down microVM", "workflow", wid)
	}
}

func debugSSHCommand(listenAddr, hostname, debugHost, jumpHost, jobID string) string {
	host, port := hostname, ""
	if debugHost != "" {
		host = debugHost
	}
	if _, p, err := net.SplitHostPort(listenAddr); err == nil {
		port = p
	}
	args := []string{"ssh", "-tt"}
	if jumpHost != "" {
		args = append(args, "-J", jumpHost)
	}
	if port != "" && port != "22" {
		args = append(args, "-p", port)
	}
	args = append(args, jobID+"@"+host)
	return strings.Join(args, " ")
}

// bridges an interactive shell over the agentproto vsock.
// Read to it gets the shell output from guest, Write sends the keyboard input from user.
type DebugSession struct {
	conn net.Conn
	ln   net.Listener
	enc  *agentproto.Encoder
	dec  *agentproto.Decoder
	l    *slog.Logger

	out      chan []byte
	leftover []byte
	exitCode int
	closeOne sync.Once
}

func newDebugSession(conn net.Conn, ln net.Listener, l *slog.Logger) *DebugSession {
	d := &DebugSession{
		conn: conn,
		ln:   ln,
		enc:  agentproto.NewEncoder(conn),
		dec:  agentproto.NewDecoder(conn),
		l:    l,
		out:  make(chan []byte, 16),
	}
	go d.readLoop()
	return d
}

func (d *DebugSession) readLoop() {
	defer close(d.out)
	for {
		msg, err := d.dec.Decode()
		if err != nil {
			if !errors.Is(err, io.EOF) {
				d.l.Debug("debug shell decode ended", "error", err)
			}
			return
		}
		if p := msg.PtyData; p != nil && len(p.Data) > 0 {
			d.out <- p.Data
		} else if p := msg.ExecExit; p != nil {
			d.exitCode = int(p.ExitCode)
			return
		}
	}
}

func (d *DebugSession) Read(p []byte) (int, error) {
	if len(d.leftover) == 0 {
		chunk, ok := <-d.out
		if !ok {
			return 0, io.EOF
		}
		d.leftover = chunk
	}
	n := copy(p, d.leftover)
	d.leftover = d.leftover[n:]
	return n, nil
}

func (d *DebugSession) Write(p []byte) (int, error) {
	if err := d.enc.Encode(&agentproto.Message{
		Id:      "pty",
		PtyData: &agentv1.PtyData{Data: append([]byte(nil), p...)},
	}); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (d *DebugSession) Resize(rows, cols int) error {
	return d.enc.Encode(&agentproto.Message{
		Id:        "pty",
		PtyResize: &agentv1.PtyResize{Rows: clampDim(rows), Cols: clampDim(cols)},
	})
}

func (d *DebugSession) ExitCode() int { return d.exitCode }

func (d *DebugSession) Close() error {
	var err error
	d.closeOne.Do(func() {
		err = d.conn.Close()
		if d.ln != nil {
			_ = d.ln.Close()
		}
	})
	return err
}

func acceptWithTimeout(ctx context.Context, ln net.Listener, timeout time.Duration) (net.Conn, error) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	type result struct {
		conn net.Conn
		err  error
	}
	ch := make(chan result, 1)
	go func() {
		conn, err := ln.Accept()
		ch <- result{conn, err}
	}()

	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case r := <-ch:
		return r.conn, r.err
	}
}

func clampDim(v int) uint32 {
	if v < 1 {
		return 1
	}
	if v > 65535 {
		return 65535
	}
	return uint32(v)
}
