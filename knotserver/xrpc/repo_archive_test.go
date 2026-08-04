package xrpc

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestRepoArchiveChecksParamsBeforeResolvingTheRepo(t *testing.T) {
	for query, wantMessage := range map[string]string{
		"format=tar.xz":                  "only tar.gz and zip formats are supported",
		"ref=--output=/tmp/evil":         "ref starts with a dash",
		"prefix=../../evil":              "prefix escapes the archive root",
		"ref=refs/heads/main&format=zip": "repo parameter",
	} {
		rec := httptest.NewRecorder()
		(&Xrpc{}).RepoArchive(rec, httptest.NewRequest(http.MethodGet, "/xrpc/sh.tangled.repo.archive?"+query, nil))

		body := rec.Body.String()
		if rec.Code != http.StatusBadRequest || !strings.Contains(body, "InvalidRequest") || !strings.Contains(body, wantMessage) {
			t.Errorf("%s: status %d with body %s, want 400 InvalidRequest mentioning %q", query, rec.Code, body, wantMessage)
		}
	}
}
