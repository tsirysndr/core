package repoverify

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/samber/lo"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
	"tangled.org/core/xrpc/xrpcclient"
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
	ownership, ok := result.Ownership()
	if !ok {
		t.Fatalf("Answer = %s, want %s", result.Answer(), AnswerOwner)
	}
	if ownership.OwnerDid.String() != testOwnerDid {
		t.Errorf("OwnerDid = %q, want %q", ownership.OwnerDid, testOwnerDid)
	}
	if result.KnotURL.String() != srv.URL {
		t.Errorf("KnotURL = %q, want %q", result.KnotURL, srv.URL)
	}
}

func TestNew_AnswersFromA404(t *testing.T) {
	cases := map[string]struct {
		body string
		want Answer
	}{
		"only RepoNotFound will refute the repoDid":                      {`{"error":"RepoNotFound","message":"no such repo"}`, AnswerAbsent},
		"a knot without the route will answer with an empty body":        {"", AnswerNoRoute},
		"a 404 body without an error name can't claim the route":         {`{"message":"not found"}`, AnswerNoRoute},
		"a proxy json-ifying its own 404 will never answer for the knot": {`{"error":"NotFound","message":"no upstream"}`, AnswerNoRoute},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				if tc.body != "" {
					w.Header().Set("Content-Type", "application/json")
				}
				w.WriteHeader(http.StatusNotFound)
				_, _ = w.Write([]byte(tc.body))
			}))
			t.Cleanup(srv.Close)

			result, err := verifyKnot(t, srv.URL, true)
			if err != nil {
				t.Fatalf("a 404 from describeRepo must be an answer: %v", err)
			}
			if result.Answer() != tc.want {
				t.Errorf("Answer = %s, want %s", result.Answer(), tc.want)
			}
			if _, ok := result.Ownership(); ok {
				t.Error("a 404 won't identify an owner")
			}
		})
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

func TestRetriable(t *testing.T) {
	retriable := []error{
		errors.New("connection refused"),
		fmt.Errorf("describeRepo: %w", xrpcclient.ErrXrpcFailed),
	}
	terminal := append([]error{nil}, lo.Map(terminalErrors, func(e error, _ int) error {
		return fmt.Errorf("describeRepo: %w", e)
	})...)
	for _, err := range retriable {
		if !Retriable(err) {
			t.Errorf("Retriable(%v) = false, want true, since the knot may answer the next call", err)
		}
	}
	for _, err := range terminal {
		if Retriable(err) {
			t.Errorf("Retriable(%v) = true, want false, since the answer won't change on a retry", err)
		}
	}
}
