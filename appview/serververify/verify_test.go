package serververify

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

const ssrfExpectedOwner = "did:plc:ssrfguardexpectedowner"

func TestRunVerificationRejectsNonPublicDestinationsInProd(t *testing.T) {
	loopbackDomain, loopbackHits := localOwnerEndpoint(t, "127.0.0.1")

	cases := []struct {
		name   string
		domain string
		hits   *atomic.Int32
	}{
		{
			name:   "loopback address with a real owner endpoint",
			domain: loopbackDomain,
			hits:   loopbackHits,
		},
		{
			name:   "private address",
			domain: "10.0.0.1:80",
		},
		{
			name:   "link-local metadata address",
			domain: "169.254.169.254:80",
		},
		{
			name:   "reserved unspecified address",
			domain: "0.0.0.0:80",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if tc.hits != nil {
				tc.hits.Store(0)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 750*time.Millisecond)
			defer cancel()

			started := time.Now()
			err := RunVerification(ctx, tc.domain, ssrfExpectedOwner, false)
			elapsed := time.Since(started)

			if err == nil {
				t.Fatalf("RunVerification(%q, dev=false) succeeded; non-public destinations must be refused", tc.domain)
			}
			if elapsed > 250*time.Millisecond {
				t.Fatalf("RunVerification(%q, dev=false) took %s; want an immediate SSRF refusal, not network IO until timeout", tc.domain, elapsed)
			}
			if tc.hits != nil && tc.hits.Load() != 0 {
				t.Fatalf("RunVerification(%q, dev=false) reached the owner endpoint %d time(s); guard must refuse before normal network IO", tc.domain, tc.hits.Load())
			}
		})
	}
}

func TestRunVerificationAllowsNonPublicDestinationsInDev(t *testing.T) {
	loopbackDomain, loopbackHits := localOwnerEndpoint(t, "127.0.0.1")

	ctx, cancel := context.WithTimeout(context.Background(), 750*time.Millisecond)
	defer cancel()

	err := RunVerification(ctx, loopbackDomain, ssrfExpectedOwner, true)
	if err != nil {
		t.Fatalf("RunVerification(%q, dev=true) failed: %v", loopbackDomain, err)
	}

	if loopbackHits.Load() != 1 {
		t.Fatalf("RunVerification(%q, dev=true) did not reach owner endpoint", loopbackDomain)
	}
}

func localOwnerEndpoint(t *testing.T, host string) (string, *atomic.Int32) {
	t.Helper()

	ln, err := net.Listen("tcp", net.JoinHostPort(host, "0"))
	if err != nil {
		t.Fatalf("listen on %s: %v", host, err)
	}

	var hits atomic.Int32
	server := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		if r.URL.Path != "/xrpc/sh.tangled.owner" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"owner":%q}`, ssrfExpectedOwner)
	}))
	server.Listener = ln
	server.Start()
	t.Cleanup(server.Close)

	return ln.Addr().String(), &hits
}
