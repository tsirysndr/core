package netutil

import (
	"fmt"
	"net"
	"net/http"
	"net/url"

	"github.com/bluesky-social/indigo/util/ssrf"
	"github.com/gorilla/websocket"
)

// refuses non-public ips to prevent ssrf
func SSRFDialer(dev bool) *net.Dialer {
	if dev {
		return &net.Dialer{}
	}
	return ssrf.PublicOnlyDialer()
}

// refuses non-public ips to prevent ssrf
func SSRFTransport(dev bool) *http.Transport {
	if dev {
		return &http.Transport{}
	}
	return ssrf.PublicOnlyTransport()
}

// refuses non-public ips to prevent ssrf
func SSRFWebsocketDialer(dev bool) *websocket.Dialer {
	dialer := *websocket.DefaultDialer
	dialer.NetDialContext = SSRFDialer(dev).DialContext
	return &dialer
}

func EnforceWSSURL(rawURL string, dev bool) (*url.URL, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("invalid url: %w", err)
	}
	switch u.Scheme {
	case "wss":
	case "ws":
		if !dev {
			return nil, fmt.Errorf("insecure scheme %q is prohibited in production; use wss://", u.Scheme)
		}
	default:
		return nil, fmt.Errorf("unsupported websocket scheme %q", u.Scheme)
	}
	return u, nil
}
