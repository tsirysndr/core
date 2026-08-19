//go:build linux

package microvm

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/pem"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/gliderlabs/ssh"
	gossh "golang.org/x/crypto/ssh"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hostutil"
)

// debug ssh: terminates ssh here, bridges a pty into a live (failed) microVM
// over the guest's agent conn. the guest stays keyless. access mirrors git
// push: the offered key goes to the repo's knot
// (sh.tangled.repo.checkPushAllowed) and is accepted only if it can push to the
// job's repo.

const debugAuthTimeout = 5 * time.Second

func (e *Engine) serveDebugSSH(ctx context.Context) {
	dbg := e.cfg.MicroVMPipelines.DebugSSH
	addr := dbg.ListenAddr
	httpc := &http.Client{Timeout: debugAuthTimeout}

	srv := &ssh.Server{
		Addr:    addr,
		Handler: e.debugHandle,
		PublicKeyHandler: func(c ssh.Context, key ssh.PublicKey) bool {
			return e.checkDebugAuth(c, c.User(), key, httpc)
		},
	}
	keyPath := e.cfg.MicroVMPipelines.DebugSSH.HostKeyPath
	if keyPath == "" {
		keyPath = filepath.Join(filepath.Dir(e.cfg.Server.DBPath), "debug_ssh_host_key")
	}
	if err := ensureDebugHostKey(keyPath); err != nil {
		e.l.Error("debug ssh: ensure host key", "path", keyPath, "err", err)
		return
	}
	if err := srv.SetOption(ssh.HostKeyFile(keyPath)); err != nil {
		e.l.Error("debug ssh: load host key", "path", keyPath, "err", err)
		return
	}

	go func() {
		<-ctx.Done()
		_ = srv.Close()
	}()

	e.l.Info("starting debug ssh server", "address", addr)
	if err := srv.ListenAndServe(); err != nil && err != ssh.ErrServerClosed {
		e.l.Error("debug ssh server stopped", "err", err)
	}
}

func ensureDebugHostKey(path string) error {
	if _, err := os.Stat(path); err == nil {
		return nil
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("stat host key: %w", err)
	}

	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return fmt.Errorf("generate host key: %w", err)
	}
	block, err := gossh.MarshalPrivateKey(priv, "")
	if err != nil {
		return fmt.Errorf("marshal host key: %w", err)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("create host key dir: %w", err)
	}
	if err := os.WriteFile(path, pem.EncodeToMemory(block), 0o600); err != nil {
		return fmt.Errorf("write host key: %w", err)
	}
	return nil
}

func (e *Engine) debugHandle(sess ssh.Session) {
	ptyReq, winCh, isPty := sess.Pty()
	if !isPty {
		io.WriteString(sess.Stderr(), "error: no terminal allocated; use `ssh -t`\n")
		_ = sess.Exit(1)
		return
	}

	jobID := sess.User()
	l := e.l.With("component", "debugssh", "job", jobID)

	debug, err := e.OpenDebugSession(sess.Context(), jobID, ptyReq.Term, ptyReq.Window.Height, ptyReq.Window.Width)
	if err != nil {
		fmt.Fprintf(sess.Stderr(), "error: %v\n", err)
		_ = sess.Exit(1)
		return
	}
	defer debug.Close()
	l.Info("debug shell opened")

	go func() {
		for win := range winCh {
			if err := debug.Resize(win.Height, win.Width); err != nil {
				l.Debug("debug ssh resize failed", "error", err)
			}
		}
	}()

	// keyboard -> shell, runs until the client hangs up
	go func() { _, _ = io.Copy(debug, sess) }()
	// shell -> client, returns when the shell exits (Read hits EOF)
	_, _ = io.Copy(sess, debug)

	code := debug.ExitCode()
	l.Info("debug shell closed", "exitCode", code)
	// the user is done; let retention tear the VM down now instead of waiting
	// out the rest of the grace period
	e.releaseDebugTarget(jobID)
	_ = sess.Exit(code)
}

func (e *Engine) checkDebugAuth(ctx context.Context, jobID string, key ssh.PublicKey, httpc *http.Client) bool {
	l := e.l.With("component", "debugssh", "job", jobID, "keyType", key.Type())

	knot, repoDid, ok := e.RepoForJob(jobID)
	if !ok {
		l.Warn("debug ssh: no live job / unknown repo")
		return false
	}

	host, noSSL, err := hostutil.ParseHostname(knot)
	if err != nil {
		l.Error("debug ssh: bad knot host", "knot", knot, "error", err)
		return false
	}
	scheme := "https"
	if noSSL {
		scheme = "http"
	}
	xc := &indigoxrpc.Client{Host: fmt.Sprintf("%s://%s", scheme, host), Client: httpc}

	reqCtx, cancel := context.WithTimeout(ctx, debugAuthTimeout)
	defer cancel()

	out, err := tangled.RepoCheckPushAllowed(reqCtx, xc, string(gossh.MarshalAuthorizedKey(key)), repoDid)
	if err != nil {
		l.Error("debug ssh: push-allowed check failed", "knot", knot, "repo", repoDid, "error", err)
		return false
	}
	if !out.Allowed {
		l.Warn("debug ssh: key not allowed to push", "knot", knot, "repo", repoDid)
		return false
	}
	if out.Did != nil {
		l.Info("debug ssh: authorized", "did", *out.Did, "knot", knot, "repo", repoDid)
	}
	return true
}
