package mill

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/pem"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/gliderlabs/ssh"
	gossh "golang.org/x/crypto/ssh"
)

const (
	jumpIdleTimeout         = 5 * time.Minute
	jumpMaxTimeout          = 24 * time.Hour
	maxJumpConnectionsPerIP = 8
)

type jumpContextKey string

const jumpRouteOpened jumpContextKey = "route-opened"

func (m *Mill) ServeJump(ctx context.Context, listenAddr, hostKeyPath string, executorPort uint32, maxConnections int) {
	if listenAddr == "" {
		return
	}
	srv, err := m.newJumpServer(hostKeyPath, executorPort, maxConnections)
	if err != nil {
		m.l.Error("setup debug ssh jump server", "err", err)
		return
	}

	go func() {
		<-ctx.Done()
		_ = srv.Close()
	}()

	m.l.Info("starting debug ssh jump server", "address", listenAddr)
	srv.Addr = listenAddr
	if err := srv.ListenAndServe(); err != nil && err != ssh.ErrServerClosed {
		m.l.Error("debug ssh jump server stopped", "err", err)
	}
}

func (m *Mill) newJumpServer(hostKeyPath string, executorPort uint32, maxConnections int) (*ssh.Server, error) {
	if executorPort == 0 {
		executorPort = 2223
	}
	if maxConnections <= 0 {
		return nil, fmt.Errorf("max jump connections must be greater than zero")
	}
	if err := ensureJumpHostKey(hostKeyPath); err != nil {
		return nil, fmt.Errorf("prepare jump host key: %w", err)
	}
	limiter := newJumpConnectionLimiter(maxConnections, maxJumpConnectionsPerIP)
	srv := &ssh.Server{
		PublicKeyHandler: func(ctx ssh.Context, _ ssh.PublicKey) bool {
			return ctx.User() == "debug"
		},
		ConnCallback: func(ctx ssh.Context, conn net.Conn) net.Conn {
			if !limiter.acquire(conn.RemoteAddr()) {
				_ = conn.Close()
				return conn
			}
			go func() {
				<-ctx.Done()
				limiter.release(conn.RemoteAddr())
			}()
			return conn
		},
		LocalPortForwardingCallback: func(ctx ssh.Context, host string, port uint32) bool {
			if port != executorPort || !m.hasLiveExecutor(host) {
				return false
			}
			ctx.Lock()
			defer ctx.Unlock()
			if opened, _ := ctx.Value(jumpRouteOpened).(bool); opened {
				return false
			}
			ctx.SetValue(jumpRouteOpened, true)
			return true
		},
		ChannelHandlers: map[string]ssh.ChannelHandler{
			"direct-tcpip": ssh.DirectTCPIPHandler,
		},
		IdleTimeout: jumpIdleTimeout,
		MaxTimeout:  jumpMaxTimeout,
	}
	if err := srv.SetOption(ssh.HostKeyFile(hostKeyPath)); err != nil {
		return nil, fmt.Errorf("load jump host key: %w", err)
	}
	return srv, nil
}

type jumpConnectionLimiter struct {
	mu       sync.Mutex
	total    int
	perIP    map[string]int
	maxTotal int
	maxPerIP int
}

func newJumpConnectionLimiter(maxTotal, maxPerIP int) *jumpConnectionLimiter {
	return &jumpConnectionLimiter{
		perIP:    make(map[string]int),
		maxTotal: maxTotal,
		maxPerIP: maxPerIP,
	}
}

func (l *jumpConnectionLimiter) acquire(addr net.Addr) bool {
	host := jumpRemoteHost(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.total >= l.maxTotal || l.perIP[host] >= l.maxPerIP {
		return false
	}
	l.total++
	l.perIP[host]++
	return true
}

func (l *jumpConnectionLimiter) release(addr net.Addr) {
	host := jumpRemoteHost(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	l.total--
	l.perIP[host]--
	if l.perIP[host] == 0 {
		delete(l.perIP, host)
	}
}

func jumpRemoteHost(addr net.Addr) string {
	if addr == nil {
		return ""
	}
	host, _, err := net.SplitHostPort(addr.String())
	if err != nil {
		return addr.String()
	}
	return host
}

func ensureJumpHostKey(path string) error {
	if _, err := os.Stat(path); err == nil {
		return nil
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("stat host key: %w", err)
	}

	_, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return fmt.Errorf("generate host key: %w", err)
	}
	block, err := gossh.MarshalPrivateKey(privateKey, "")
	if err != nil {
		return fmt.Errorf("marshal host key: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return fmt.Errorf("create host key directory: %w", err)
	}
	return os.WriteFile(path, pem.EncodeToMemory(block), 0o600)
}
