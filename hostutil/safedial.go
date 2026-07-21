package hostutil

import (
	"fmt"
	"net"
	"net/http"
	"net/url"
	"syscall"
	"time"
)

// isBlockedIP reports whether ip is loopback, private, link-local (incl. the
// 169.254.169.254 metadata endpoint), multicast, or unspecified.
func isBlockedIP(ip net.IP) bool {
	return ip.IsLoopback() ||
		ip.IsPrivate() ||
		ip.IsLinkLocalUnicast() ||
		ip.IsLinkLocalMulticast() ||
		ip.IsMulticast() ||
		ip.IsUnspecified()
}

// safeDialer rejects dials to non-public addresses. the Control hook runs after
// dns resolution, so it also covers rebinding and redirects. disabled in dev.
func safeDialer(dev bool) *net.Dialer {
	d := &net.Dialer{
		Timeout:   10 * time.Second,
		KeepAlive: 30 * time.Second,
	}
	if dev {
		return d
	}
	d.Control = func(_, address string, _ syscall.RawConn) error {
		host, _, err := net.SplitHostPort(address)
		if err != nil {
			return fmt.Errorf("invalid dial address %q: %w", address, err)
		}
		ip := net.ParseIP(host)
		if ip == nil {
			return fmt.Errorf("dial address %q did not resolve to an IP", address)
		}
		if isBlockedIP(ip) {
			return fmt.Errorf("refusing to dial %s: reserved or private address", ip)
		}
		return nil
	}
	return d
}

// ValidateExternalURL checks raw is a well-formed http(s) url and rejects
// ip-literal hosts in blocked ranges; dns hosts are re-checked at dial time.
func ValidateExternalURL(raw string, dev bool) error {
	u, err := url.Parse(raw)
	if err != nil {
		return fmt.Errorf("invalid URL: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return fmt.Errorf("URL must use http or https")
	}
	if u.Hostname() == "" {
		return fmt.Errorf("URL must include a host")
	}
	if dev {
		return nil
	}
	if ip := net.ParseIP(u.Hostname()); ip != nil && isBlockedIP(ip) {
		return fmt.Errorf("URL host is a reserved or private address")
	}
	return nil
}

// SafeClient returns an http.Client for fetching untrusted urls (e.g.
// webhooks): it blocks internal address ranges and won't follow redirects.
func SafeClient(dev bool, timeout time.Duration) *http.Client {
	return &http.Client{
		Timeout: timeout,
		Transport: &http.Transport{
			DialContext: safeDialer(dev).DialContext,
		},
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
}
