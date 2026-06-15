package codesearch

import (
	"testing"

	"github.com/sourcegraph/zoekt/query"
)

func TestAsRepoSearch(t *testing.T) {
	cases := []struct {
		query   string
		wantOK  bool
		wantStr string // rewritten repo-search query, only checked when wantOK
	}{
		{"repo:foo", true, "foo"},
		{"repo:foo repo:bar", true, "foo bar"},
		{"repo:foo lang:go", true, "foo lang:Go"},
		{"lang:go", false, ""},
		{"repo:foo bar", false, ""},
		{"repo:foo file:x", false, ""},
		{"file:x", false, ""},
		{"type:repo foo", true, "foo"},
		{"foo", false, ""},
		{"branch:main", false, ""},
	}

	for _, tc := range cases {
		t.Run(tc.query, func(t *testing.T) {
			q, err := query.Parse(tc.query)
			if err != nil {
				t.Fatalf("parse %q: %v", tc.query, err)
			}
			rs, ok := asRepoSearch(q)
			if ok != tc.wantOK {
				t.Fatalf("asRepoSearch(%q) ok = %v, want %v", tc.query, ok, tc.wantOK)
			}
			if ok && rs.Query() != tc.wantStr {
				t.Errorf("asRepoSearch(%q).Query() = %q, want %q", tc.query, rs.Query(), tc.wantStr)
			}
		})
	}
}

func TestExtractDID(t *testing.T) {
	cases := []struct {
		tmpl string
		want string
	}{
		{"https://tangled.org/did:plc:abc123/blob/{{.Version}}/{{.Path}}", "did:plc:abc123"},
		{"http://localhost:3000/did:web:example.com/blob/{{.Version}}/{{.Path}}", "did:web:example.com"},
		{"", ""},
	}

	for _, tc := range cases {
		t.Run(tc.tmpl, func(t *testing.T) {
			if got := extractDID(tc.tmpl); string(got) != tc.want {
				t.Errorf("extractDID(%q) = %q, want %q", tc.tmpl, got, tc.want)
			}
		})
	}
}
