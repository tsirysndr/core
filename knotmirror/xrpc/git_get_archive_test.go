package xrpc

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestGetArchiveRejectsBadParams(t *testing.T) {
	for query, wantMessage := range map[string]string{
		"":                                        "repo parameter invalid",
		"repo=oyster.cafe%2Fsquid":                "repo parameter invalid",
		"repo=did:plc:boltless&format=tar.xz":     "only tar.gz and zip formats are supported",
		"repo=did:plc:boltless&ref=--output=/x":   "ref starts with a dash",
		"repo=did:plc:boltless&prefix=../../evil": "prefix escapes the archive root",
	} {
		rec := httptest.NewRecorder()
		(&Xrpc{}).GetArchive(rec, httptest.NewRequest(http.MethodGet, "/xrpc/sh.tangled.git.temp.getArchive?"+query, nil))

		body := rec.Body.String()
		if rec.Code != http.StatusBadRequest || !strings.Contains(body, "InvalidRequest") || !strings.Contains(body, wantMessage) {
			t.Errorf("%s: status %d with body %s, want 400 InvalidRequest mentioning %q", query, rec.Code, body, wantMessage)
		}
	}
}
