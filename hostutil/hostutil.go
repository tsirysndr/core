package hostutil

import (
	"fmt"
	"net/url"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func ParseHostname(raw string) (hostname string, noSSL bool, err error) {
	// handle case of bare hostname
	if !strings.Contains(raw, "://") {
		if strings.HasPrefix(raw, "localhost:") {
			raw = "http://" + raw
		} else {
			raw = "https://" + raw
		}
	}

	u, err := url.Parse(raw)
	if err != nil {
		return "", false, fmt.Errorf("not a valid host URL: %w", err)
	}

	switch u.Scheme {
	case "https", "wss":
		noSSL = false
	case "http", "ws":
		noSSL = true
	default:
		return "", false, fmt.Errorf("unsupported URL scheme: %s", u.Scheme)
	}

	// 'localhost' (exact string) is allowed *with* a required port number; SSL is optional
	if u.Hostname() == "localhost" {
		if u.Port() == "" || !strings.HasPrefix(u.Host, "localhost:") {
			return "", false, fmt.Errorf("port number is required for localhost")
		}
		return u.Host, noSSL, nil
	}

	// port numbers not allowed otherwise
	if u.Port() != "" {
		return "", false, fmt.Errorf("port number not allowed for non-local names")
	}

	// check it is a real hostname (eg, not IP address or single-word alias)
	h, err := syntax.ParseHandle(u.Host)
	if err != nil {
		return "", false, fmt.Errorf("not a public hostname")
	}

	// lower-case in response
	return h.Normalize().String(), noSSL, nil
}

func EnsureHttpScheme(host string) (string, error) {
	hostname, noSSL, err := ParseHostname(host)
	if err != nil {
		return "", err
	}
	if noSSL {
		return "http://" + hostname, nil
	} else {
		return "https://" + hostname, nil
	}
}

func EnsureWsScheme(host string) (string, error) {
	hostname, noSSL, err := ParseHostname(host)
	if err != nil {
		return "", err
	}
	if noSSL {
		return "ws://" + hostname, nil
	} else {
		return "wss://" + hostname, nil
	}
}
