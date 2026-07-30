package repoverify

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/bluesky-social/indigo/atproto/identity"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
)

const (
	testRepoDid  = repoident.RepoDid("did:plc:limpet")
	testOwnerDid = "did:plc:akshay"
)

func describeRepoServer(t *testing.T, describedRepoDid repoident.RepoDid) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/xrpc/"+tangled.RepoDescribeRepoNSID {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(tangled.RepoDescribeRepo_Output{
			RepoDid:  describedRepoDid.String(),
			OwnerDid: testOwnerDid,
			Rkey:     "3kkkkkkkkkkkk",
		})
	}))
	t.Cleanup(srv.Close)
	return srv
}

func verifyKnot(t *testing.T, knotURL string, dev bool) (Result, error) {
	t.Helper()
	resolver := idresolver.NewMockResolver(idresolver.MockDirectory{Ident: &identity.Identity{
		Services: map[string]identity.ServiceEndpoint{
			repoident.KnotServiceID: {Type: repoident.KnotServiceType, URL: knotURL},
		},
	}})
	return New(resolver, dev)(context.Background(), testRepoDid)
}

func TestNew_DevModeAcceptsHttpKnotEndpointAndStripsThePath(t *testing.T) {
	srv := describeRepoServer(t, testRepoDid)

	result, err := verifyKnot(t, srv.URL+"/repo/m5326fp3qemiriiqypxv6rrhai", true)
	if err != nil {
		t.Fatalf("dev mode should accept an http knot endpoint: %v", err)
	}
	if result.OwnerDid.String() != testOwnerDid {
		t.Errorf("OwnerDid = %q, want %q", result.OwnerDid, testOwnerDid)
	}
	if result.KnotURL.String() != srv.URL {
		t.Errorf("KnotURL = %q, want %q", result.KnotURL, srv.URL)
	}
}

func TestNew_Rejections(t *testing.T) {
	target := describeRepoServer(t, testRepoDid)
	otherRepo := describeRepoServer(t, "did:plc:anemone")
	redirector := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL+r.URL.Path, http.StatusFound)
	}))
	t.Cleanup(redirector.Close)

	cases := map[string]struct {
		knotURL  string
		dev      bool
		want     string
		sentinel error
	}{
		"http endpoint outside dev mode":        {target.URL, false, "must use https", nil},
		"knot redirects elsewhere":              {redirector.URL, true, "describeRepo on", nil},
		"identity declares no knot":             {"", true, "", repoident.ErrNoKnotService},
		"describeRepo answers for another repo": {otherRepo.URL, true, `repoDid "did:plc:anemone"`, ErrKnotAnswer},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			_, err := verifyKnot(t, tc.knotURL, tc.dev)
			if err == nil {
				t.Fatalf("verify accepted a knot it should reject")
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Errorf("error = %v, want one mentioning %q", err, tc.want)
			}
			if tc.sentinel != nil && !errors.Is(err, tc.sentinel) {
				t.Errorf("error = %v, want one matching %v", err, tc.sentinel)
			}
		})
	}
}
