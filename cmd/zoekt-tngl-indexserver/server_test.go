package main

import (
	"fmt"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/repoident"
)

var (
	sha1Oid   = strings.Repeat("a", 40)
	sha256Oid = strings.Repeat("b", 64)
)

func decodeBody(t *testing.T, body string) (indexRequest, error) {
	t.Helper()
	return decodeIndexRequest(httptest.NewRequest("POST", "/admin/enqueueIndex", strings.NewReader(body)))
}

func TestDecodeIndexRequest_Accepts(t *testing.T) {
	cases := map[string]struct {
		name, oid, wantRef string
	}{
		"a branch and a sha1": {name: "main", oid: sha1Oid, wantRef: "refs/heads/main"},
		"HEAD and a sha256":   {name: "HEAD", oid: sha256Oid, wantRef: "HEAD"},
	}
	for label, tc := range cases {
		t.Run(label, func(t *testing.T) {
			req, err := decodeBody(t, fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":%q,"Version":%q}]}`, tc.name, tc.oid))
			if err != nil {
				t.Fatalf("decodeIndexRequest: %v", err)
			}
			if req.Repo.String() != "did:plc:limpet" {
				t.Errorf("Repo = %q, want did:plc:limpet", req.Repo)
			}
			want := indexBranch{Name: branchName(tc.name), Version: objectID(tc.oid)}
			if len(req.Branches) != 1 || req.Branches[0] != want {
				t.Errorf("Branches = %v, want %v", req.Branches, want)
			}
			if got := req.Branches[0].Name.Ref(); got != tc.wantRef {
				t.Errorf("Ref = %q, want %q", got, tc.wantRef)
			}
		})
	}
}

func TestDecodeIndexRequest_RejectsBadRequests(t *testing.T) {
	cases := map[string]string{
		"branch name is a git option": fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":"-d","Version":%q}]}`, sha1Oid),
		"version is a git option":     `{"repo":"did:plc:limpet","branches":[{"Name":"main","Version":"--upload-pack=touch /tmp/pwned"}]}`,
		"version is a ref":            `{"repo":"did:plc:limpet","branches":[{"Name":"main","Version":"refs/heads/main"}]}`,
		"version is short hex":        `{"repo":"did:plc:limpet","branches":[{"Name":"main","Version":"deadbeef"}]}`,
		"branch name walks up":        fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":"../../objects","Version":%q}]}`, sha1Oid),
		"branch name is a full ref":   fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":"refs/heads/main","Version":%q}]}`, sha1Oid),
		"branch name is empty":        fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":"","Version":%q}]}`, sha1Oid),
		"repo isn't a did":            fmt.Sprintf(`{"repo":"limpet","branches":[{"Name":"main","Version":%q}]}`, sha1Oid),
		"repo is absent":              fmt.Sprintf(`{"branches":[{"Name":"main","Version":%q}]}`, sha1Oid),
		"branches are absent":         `{"repo":"did:plc:limpet"}`,
		"branches are empty":          `{"repo":"did:plc:limpet","branches":[]}`,
		"unknown field":               fmt.Sprintf(`{"repo":"did:plc:limpet","branches":[{"Name":"main","Version":%q}],"shards":3}`, sha1Oid),
	}
	for name, body := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := decodeBody(t, body); err == nil {
				t.Errorf("decodeIndexRequest accepted %s", body)
			}
		})
	}
}

func TestRepoCloneURL(t *testing.T) {
	knot, err := repoident.ParseKnotURL("https://knot.oyster.cafe", repoident.RequireHTTPS)
	if err != nil {
		t.Fatalf("ParseKnotURL: %v", err)
	}
	repo := Repo{Did: "did:plc:limpet", Owner: "did:plc:akshay", Slug: syntax.RecordKey("3kkkkkkkkkkkk"), Knot: knot}
	const want = "https://knot.oyster.cafe/did:plc:limpet"
	if got := repo.CloneURL(); got != want {
		t.Errorf("CloneURL = %q, want %q", got, want)
	}
}
