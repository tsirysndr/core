package xrpc

import (
	"net/http/httptest"
	"testing"

	"github.com/stretchr/testify/assert"
)

func TestForwardedForAppendsThePeerToTheChain(t *testing.T) {
	cases := []struct {
		name   string
		remote string
		chain  []string
		want   string
	}{
		{"a direct caller is the whole chain", "203.0.113.7:52344", nil, "203.0.113.7"},
		{"a portless remote address passes through", "203.0.113.7", nil, "203.0.113.7"},
		{"bobbin's client address stays left of bobbin", "198.51.100.4:41000", []string{"203.0.113.7"}, "203.0.113.7, 198.51.100.4"},
		{"a chain split across header lines joins into a single value", "198.51.100.4:41000", []string{"203.0.113.7", "192.0.2.9"}, "203.0.113.7, 192.0.2.9, 198.51.100.4"},
		{"a blank entry never leaves a gap the knot has to skip", "198.51.100.4:41000", []string{"", "   ", "203.0.113.7"}, "203.0.113.7, 198.51.100.4"},
		{"an ipv6 peer loses its port and keeps its colons", "[2001:db8::5]:41000", []string{"203.0.113.7"}, "203.0.113.7, 2001:db8::5"},
		{"a forged entry stays left of the address that sent it", "203.0.113.7:52344", []string{"192.0.2.9"}, "192.0.2.9, 203.0.113.7"},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			r := httptest.NewRequest("GET", "/xrpc/sh.tangled.git.temp.getTree?repo=did:plc:limpet", nil)
			r.RemoteAddr = c.remote
			for _, entry := range c.chain {
				r.Header.Add(forwardedForHeader, entry)
			}

			assert.Equal(t, c.want, forwardedFor(r))
			assert.Equal(t, c.chain, r.Header.Values(forwardedForHeader), "the caller's own header must survive the read")
		})
	}
}
