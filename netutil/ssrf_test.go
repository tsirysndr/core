package netutil

import (
	"testing"

	"github.com/gorilla/websocket"
)

func TestSSRFWebsocketDialerPreservesHandshakeTimeout(t *testing.T) {
	dialer := SSRFWebsocketDialer(false)
	if dialer.HandshakeTimeout != websocket.DefaultDialer.HandshakeTimeout {
		t.Fatalf("HandshakeTimeout = %v, want %v", dialer.HandshakeTimeout, websocket.DefaultDialer.HandshakeTimeout)
	}
	if dialer.NetDialContext == nil {
		t.Fatal("NetDialContext is nil; public-only dialing is not enforced")
	}
}
