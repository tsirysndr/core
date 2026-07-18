package mill

import (
	"github.com/gorilla/websocket"
	"io"
	"net/http"
	"slices"
	"strings"
	"sync"
	"time"

	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

var (
	handshakeSem       = make(chan struct{}, 16)
	handshakeMu        sync.Mutex
	inFlightHandshakes = make(map[string]struct{})
)

type livenessReader struct {
	r           io.Reader
	conn        *websocket.Conn
	readTimeout time.Duration
}

func (lr *livenessReader) Read(p []byte) (int, error) {
	if err := lr.conn.SetReadDeadline(time.Now().Add(lr.readTimeout)); err != nil {
		return 0, err
	}
	return lr.r.Read(p)
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

// auth before upgrade, a bad token never opens a socket
func (m *Mill) HandleExecutorConn(w http.ResponseWriter, r *http.Request) {
	name, authorizedLabels, ok := m.authenticate(r)
	if !ok {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	select {
	case handshakeSem <- struct{}{}:
	case <-r.Context().Done():
		return
	}
	handshakeSlotHeld := true
	defer func() {
		if handshakeSlotHeld {
			<-handshakeSem
		}
	}()

	// enforces one in-flight handshake and one live session at a time per identity
	handshakeMu.Lock()
	if _, ok := inFlightHandshakes[name]; ok {
		handshakeMu.Unlock()
		http.Error(w, "handshake already in progress", http.StatusConflict)
		return
	}
	m.mu.Lock()
	old, exists := m.sessions[name]
	isLive := exists && old.live(m.cfg.ReconnectGrace)
	m.mu.Unlock()
	if isLive {
		handshakeMu.Unlock()
		http.Error(w, "session already active", http.StatusConflict)
		return
	}
	inFlightHandshakes[name] = struct{}{}
	handshakeMu.Unlock()
	identityHandshakeHeld := true

	defer func() {
		if identityHandshakeHeld {
			handshakeMu.Lock()
			delete(inFlightHandshakes, name)
			handshakeMu.Unlock()
		}
	}()

	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		m.l.Error("fleet ws upgrade failed", "err", err)
		return
	}
	defer conn.Close()

	if err := conn.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
		m.l.Error("failed to set pre-hello read deadline", "err", err)
		return
	}

	stream := millproto.NewWSStream(conn)
	enc := millproto.NewEncoder(stream)
	dec := millproto.NewDecoder(stream)

	hello, err := dec.Decode()
	if err != nil {
		m.l.Error("fleet read hello failed", "err", err)
		return
	}
	h := hello.GetHello()
	if h == nil {
		m.l.Error("fleet first frame was not hello")
		return
	}
	if h.GetProtocolVersion() != millproto.ProtocolVersion {
		m.l.Error("fleet protocol version mismatch", "got", h.GetProtocolVersion(), "want", millproto.ProtocolVersion)
		return
	}
	if h.GetEpoch() == "" {
		m.l.Error("fleet hello missing epoch")
		return
	}

	for _, l := range h.GetLabels() {
		if !slices.Contains(authorizedLabels, l) {
			m.l.Error("executor requested unauthorized label", "label", l, "authorized", authorizedLabels)
			return
		}
	}

	sess := newSession(name, h.GetEpoch(), authorizedLabels, enc, m.l)
	sess.closeTransport = conn.Close
	sess.labels = h.GetLabels()

	resume, ok := m.attachSession(sess)
	if !ok {
		m.l.Warn("rejecting duplicate live executor session", "node", name)
		return
	}
	m.l.Info("executor connected", "node", sess.nodeID, "arch", h.GetArch(), "labels", h.GetLabels(), "resume", resume)

	if err := sess.send(&millproto.Message{Resume: &millv1.Resume{Epoch: h.GetEpoch(), AckSeqno: resume}}); err != nil {
		m.l.Error("fleet send resume failed", "err", err)
		m.detachSession(sess)
		return
	}
	handshakeSlotHeld = false
	<-handshakeSem
	handshakeMu.Lock()
	delete(inFlightHandshakes, name)
	handshakeMu.Unlock()
	identityHandshakeHeld = false
	m.sessionReady(sess)

	readTimeout := m.cfg.ReconnectGrace
	if readTimeout <= 0 {
		readTimeout = 45 * time.Second
	}
	liveDec := millproto.NewDecoder(&livenessReader{r: stream, conn: conn, readTimeout: readTimeout})

	if err := sess.readLoop(m, liveDec); err != nil {
		m.l.Debug("session read ended", "node", sess.nodeID, "err", err)
		m.noteSessionError(sess, err)
	}
	m.detachSession(sess)
}

// identity comes from the token hash. unknown or missing token fails closed
func (m *Mill) authenticate(r *http.Request) (string, []string, bool) {
	const prefix = "Bearer "
	h := r.Header.Get("Authorization")
	if !strings.HasPrefix(h, prefix) {
		return "", nil, false
	}
	token := strings.TrimPrefix(h, prefix)
	if token == "" || m.db == nil {
		return "", nil, false
	}
	name, labels, ok, err := m.db.ResolveExecutorToken(HashToken(token))
	if err != nil {
		m.l.Error("executor token lookup failed", "err", err)
		return "", nil, false
	}
	return name, labels, ok
}
