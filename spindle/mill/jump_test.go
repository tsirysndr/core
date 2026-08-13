package mill

import (
	"io"
	"log/slog"
	"net"
	"os"
	"strconv"
	"testing"
	"time"

	gossh "golang.org/x/crypto/ssh"
)

func TestJumpConnectionLimiter(t *testing.T) {
	limiter := newJumpConnectionLimiter(2, 1)
	first := &net.TCPAddr{IP: net.ParseIP("192.0.2.1"), Port: 1000}
	sameIP := &net.TCPAddr{IP: net.ParseIP("192.0.2.1"), Port: 1001}
	second := &net.TCPAddr{IP: net.ParseIP("192.0.2.2"), Port: 1000}
	third := &net.TCPAddr{IP: net.ParseIP("192.0.2.3"), Port: 1000}

	if !limiter.acquire(first) {
		t.Fatal("rejected first connection")
	}
	if limiter.acquire(sameIP) {
		t.Fatal("accepted a second connection from an exhausted IP")
	}
	if !limiter.acquire(second) {
		t.Fatal("rejected connection within the global limit")
	}
	if limiter.acquire(third) {
		t.Fatal("accepted a connection beyond the global limit")
	}
	limiter.release(first)
	if !limiter.acquire(sameIP) {
		t.Fatal("did not release the per-IP slot")
	}
}

func TestJumpServerForwardsOnlyLiveExecutorRoute(t *testing.T) {
	backend := startJumpBackend(t)
	_, portText, err := net.SplitHostPort(backend.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	port, err := strconv.ParseUint(portText, 10, 32)
	if err != nil {
		t.Fatal(err)
	}

	m := New(slog.New(slog.NewTextHandler(io.Discard, nil)), Config{})
	m.sessions["127.0.0.1"] = newSession("127.0.0.1", "epoch", nil, nil, m.l)

	hostKeyPath := t.TempDir() + "/host-key"
	srv, err := m.newJumpServer(hostKeyPath, uint32(port), 2)
	if err != nil {
		t.Fatal(err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	go func() { _ = srv.Serve(listener) }()
	t.Cleanup(func() { _ = srv.Close() })

	if unauthorized, err := dialJump(t, listener.Addr().String(), "operator"); err == nil {
		_ = unauthorized.Close()
		t.Fatal("jump server accepted a non-debug user")
	}

	client := jumpClient(t, listener.Addr().String())
	defer client.Close()

	if _, err := client.Dial("tcp", net.JoinHostPort("missing", portText)); err == nil {
		t.Fatal("forwarded an executor without a live mill session")
	}
	wrongPort := strconv.FormatUint(port+1, 10)
	if _, err := client.Dial("tcp", net.JoinHostPort("127.0.0.1", wrongPort)); err == nil {
		t.Fatal("forwarded an executor on an unconfigured port")
	}

	conn, err := client.Dial("tcp", net.JoinHostPort("127.0.0.1", portText))
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	if _, err := conn.Write([]byte("hello")); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, 5)
	if _, err := io.ReadFull(conn, got); err != nil {
		t.Fatal(err)
	}
	if string(got) != "hello" {
		t.Fatalf("forwarded payload = %q", got)
	}
	if _, err := client.Dial("tcp", net.JoinHostPort("127.0.0.1", portText)); err == nil {
		t.Fatal("forwarded a second route on one jump connection")
	}
}

func startJumpBackend(t *testing.T) net.Listener {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func() {
				defer conn.Close()
				_, _ = io.Copy(conn, conn)
			}()
		}
	}()
	return listener
}

func jumpClient(t *testing.T, address string) *gossh.Client {
	t.Helper()
	client, err := dialJump(t, address, "debug")
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func dialJump(t *testing.T, address, user string) (*gossh.Client, error) {
	t.Helper()
	privateKey, err := gossh.ParsePrivateKey(testPrivateKey(t))
	if err != nil {
		t.Fatal(err)
	}
	return gossh.Dial("tcp", address, &gossh.ClientConfig{
		User:            user,
		Auth:            []gossh.AuthMethod{gossh.PublicKeys(privateKey)},
		HostKeyCallback: gossh.InsecureIgnoreHostKey(),
		Timeout:         5 * time.Second,
	})
}

func testPrivateKey(t *testing.T) []byte {
	t.Helper()
	path := t.TempDir() + "/key"
	if err := ensureJumpHostKey(path); err != nil {
		t.Fatal(err)
	}
	key, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return key
}
