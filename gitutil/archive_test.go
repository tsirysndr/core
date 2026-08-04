package gitutil

import (
	"archive/zip"
	"bytes"
	"context"
	"errors"
	"mime"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/samber/lo"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

const testArchiveEndpoint = "https://knot.nel.pet/xrpc/sh.tangled.repo.archive"

func TestParseArchiveParams(t *testing.T) {
	cases := []struct {
		name         string
		query        url.Values
		repo         RepoName
		want         ArchiveParams
		wantStem     ArchivePrefix
		wantFilename string
		wantType     string
		wantErr      string
	}{
		{"all empty", url.Values{}, "", ArchiveParams{Format: ArchiveTarGz}, "", "", "", ""},
		{"tar.gz", url.Values{"format": {"tar.gz"}}, "", ArchiveParams{Format: ArchiveTarGz}, "", "", "", ""},
		{"zip", url.Values{"format": {"zip"}}, "", ArchiveParams{Format: ArchiveZip}, "", "", "", ""},
		{
			"every param",
			url.Values{"ref": {"refs/tags/v1.0.0"}, "format": {"zip"}, "prefix": {"/kelp/"}},
			"", ArchiveParams{Rev: "refs/tags/v1.0.0", Format: ArchiveZip, Prefix: "kelp"},
			"kelp", "kelp.zip", "application/zip", "",
		},
		{"branch", url.Values{"ref": {"main"}}, "", ArchiveParams{Rev: "main", Format: ArchiveTarGz}, "squid-main", "squid-main.tar.gz", "application/gzip", ""},
		{"nested prefix in the filename", url.Values{"ref": {"main"}, "prefix": {"kelp/uni"}}, "", ArchiveParams{Rev: "main", Format: ArchiveTarGz, Prefix: "kelp/uni"}, "kelp/uni", "kelp-uni.tar.gz", "application/gzip", ""},
		{"full ref", url.Values{"ref": {"refs/heads/feat/uni"}}, "", ArchiveParams{Rev: "refs/heads/feat/uni", Format: ArchiveTarGz}, "squid-feat-uni", "squid-feat-uni.tar.gz", "application/gzip", ""},
		{"head", url.Values{"ref": {"HEAD"}}, "", ArchiveParams{Rev: "HEAD", Format: ArchiveTarGz}, "squid-HEAD", "", "", ""},
		{"trailing slash", url.Values{"ref": {"refs/heads/main/"}}, "", ArchiveParams{Rev: "refs/heads/main/", Format: ArchiveTarGz}, "squid-main-", "", "", ""},
		{"traversal in a ref", url.Values{"ref": {"../../etc"}}, "", ArchiveParams{Rev: "../../etc", Format: ArchiveTarGz}, "squid-..-..-etc", "", "", ""},
		{"windows separator in a ref", url.Values{"ref": {`feat\uni`}}, "", ArchiveParams{Rev: `feat\uni`, Format: ArchiveTarGz}, "squid-feat-uni", "", "", ""},
		{"slash in a repo name", url.Values{"ref": {"main"}}, "kelp/limpet", ArchiveParams{Rev: "main", Format: ArchiveTarGz}, "kelp-limpet-main", "kelp-limpet-main.tar.gz", "application/gzip", ""},
		{"quote in a repo name", url.Values{"ref": {"main"}}, `squid-a"b`, ArchiveParams{Rev: "main", Format: ArchiveTarGz}, `squid-a"b-main`, `squid-a"b-main.tar.gz`, "application/gzip", ""},
		{"non-ascii repo name", url.Values{"ref": {"main"}, "format": {"zip"}}, "squid-über", ArchiveParams{Rev: "main", Format: ArchiveZip}, "squid-über-main", "squid-über-main.zip", "application/zip", ""},

		{"bare slash prefix", url.Values{"prefix": {"/"}}, "", ArchiveParams{Format: ArchiveTarGz}, "", "", "", ""},
		{"did prefix", url.Values{"prefix": {"did:plc:boltless"}}, "", ArchiveParams{Format: ArchiveTarGz, Prefix: "did:plc:boltless"}, "", "", "", ""},
		{"nested prefix", url.Values{"prefix": {"squid/main"}}, "", ArchiveParams{Format: ArchiveTarGz, Prefix: "squid/main"}, "", "", "", ""},
		{"prefix wrapped in slashes", url.Values{"prefix": {"/squid/main/"}}, "", ArchiveParams{Format: ArchiveTarGz, Prefix: "squid/main"}, "", "", "", ""},
		{"space in a prefix", url.Values{"prefix": {"squid main"}}, "", ArchiveParams{Format: ArchiveTarGz, Prefix: "squid main"}, "", "", "", ""},
		{"bare dot prefix", url.Values{"prefix": {"."}}, "", ArchiveParams{Format: ArchiveTarGz}, "", "", "", ""},
		{"repeated separators and dot segments", url.Values{"prefix": {"/kelp//./uni/"}}, "", ArchiveParams{Format: ArchiveTarGz, Prefix: "kelp/uni"}, "", "", "", ""},

		{"unsupported format", url.Values{"format": {"tar"}}, "", ArchiveParams{}, "", "", "", "only tar.gz and zip formats are supported"},
		{"space in a ref", url.Values{"ref": {"refs/tags/a b"}}, "", ArchiveParams{}, "", "", "", "ref contains whitespace"},
		{"control character in a ref", url.Values{"ref": {"refs/tags/a\nb"}}, "", ArchiveParams{}, "", "", "", "ref contains whitespace"},
		{"ref that git would read as an option", url.Values{"ref": {"--output=/tmp/evil"}}, "", ArchiveParams{}, "", "", "", "ref starts with a dash"},
		{"prefix escaping the root", url.Values{"prefix": {"../../evil"}}, "", ArchiveParams{}, "", "", "", "prefix escapes the archive root"},
		{"prefix escaping below the root", url.Values{"prefix": {"squid/../../evil"}}, "", ArchiveParams{}, "", "", "", "prefix escapes the archive root"},
		{"a dot-dot segment the knot would reject", url.Values{"prefix": {"squid/../limpet"}}, "", ArchiveParams{}, "", "", "", "prefix escapes the archive root"},
		{"control character in a prefix", url.Values{"prefix": {"squid\nmain"}}, "", ArchiveParams{}, "", "", "", "prefix contains a control character"},
		{"windows separator in a prefix", url.Values{"prefix": {`..\..\evil`}}, "", ArchiveParams{}, "", "", "", "prefix contains a backslash"},
		{"prefix over the length limit", url.Values{"prefix": {strings.Repeat("a", MaxArchivePrefixLen+1)}}, "", ArchiveParams{}, "", "", "", "over the 255 byte limit"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ParseArchiveParams(tc.query)
			rejected := err != nil
			if got != tc.want || rejected != (tc.wantErr != "") || (rejected && !strings.Contains(err.Error(), tc.wantErr)) {
				t.Fatalf("params = %+v with err %v, want %+v and an error mentioning %q", got, err, tc.want, tc.wantErr)
			}
			if rejected {
				return
			}

			repo := RepoName("squid")
			if tc.repo != "" {
				repo = tc.repo
			}
			served := got.Serve(repo)
			query := served.Query("did:plc:limpet")
			if stem := ArchivePrefix(query.Get("prefix")); tc.wantStem != "" && stem != tc.wantStem {
				t.Errorf("served prefix = %q, want %q", stem, tc.wantStem)
			}
			if tc.wantFilename != "" {
				header := http.Header{}
				served.SetHeaders(header)
				mediatype, fields, err := mime.ParseMediaType(header.Get("Content-Disposition"))
				if err != nil || mediatype != "attachment" || fields["filename"] != tc.wantFilename {
					t.Errorf("Content-Disposition = %q (err %v), want an attachment with filename %q", header.Get("Content-Disposition"), err, tc.wantFilename)
				}
				if ct, sniff := header.Get("Content-Type"), header.Get("X-Content-Type-Options"); ct != tc.wantType || sniff != "nosniff" {
					t.Errorf("Content-Type = %q with X-Content-Type-Options %q, want %q and nosniff", ct, sniff, tc.wantType)
				}
			}

			if back, err := ParseArchiveParams(query); err != nil || back.Serve(repo) != served {
				t.Errorf("query round trip = %+v (err %v), want the served archive %+v", back, err, served)
			}
			back, err := ParseImmutableLink(ImmutableLink(testArchiveEndpoint + "?" + query.Encode()))
			if got.Rev != "" && (err != nil || back != got.Rev) {
				t.Errorf("Link round trip = %q (err %v), want %q", back, err, got.Rev)
			}
		})
	}
}

func TestArchiveFallbacks(t *testing.T) {
	hash := RevFromHash(plumbing.NewHash("6f1d3a2b4c5d6e7f8091a2b3c4d5e6f708192a3b"))
	if kept, filled := Rev("refs/heads/main").Or(hash), Rev("").Or(hash); kept != "refs/heads/main" || filled != hash {
		t.Errorf("Or kept %q and filled %q, want refs/heads/main and the hash %q", kept, filled, hash)
	}
	if got := Rev("").Or(RevHead); got != RevHead {
		t.Errorf("empty rev = %q, want HEAD", got)
	}
	if _, err := ParseImmutableLink(""); err == nil {
		t.Error("ParseImmutableLink must reject an empty header")
	}

	params := ArchiveParams{Rev: "main", Format: ArchiveZip, Prefix: "kelp"}
	if got := params.WithRev("6f1d3a2"); got != (ArchiveParams{Rev: "6f1d3a2", Format: ArchiveZip, Prefix: "kelp"}) || params.Rev != "main" {
		t.Errorf("WithRev = %+v leaving the receiver at %q, want only the rev replaced", got, params.Rev)
	}

	served := params.Serve("squid")
	if got := served.WithRev("6f1d3a2"); got != (ArchiveParams{Rev: "6f1d3a2", Format: ArchiveZip, Prefix: "kelp"}).Serve("squid") || served.rev != "main" {
		t.Errorf("WithRev = %+v leaving the receiver at %q, want only the rev replaced", got, served.rev)
	}

	for _, stem := range []ArchivePrefix{
		archiveStem("squid", Rev("refs/heads/"+strings.Repeat("ü", 400))),
		archiveStem(RepoName(strings.Repeat("\xff", 300)), "main"),
	} {
		_, err := ParseArchivePrefix(stem.String())
		if stem == "" || len(stem) > MaxArchivePrefixLen || !utf8.ValidString(stem.String()) || err != nil {
			t.Errorf("default prefix is %d bytes %q (err %v), want a non-empty valid prefix within %d bytes ending on a rune boundary",
				len(stem), stem, err, MaxArchivePrefixLen)
		}
	}
}

func TestRevIsObjectID(t *testing.T) {
	sha1Hex, sha256Hex := "6f1d3a2b4c5d6e7f8091a2b3c4d5e6f708192a3b", strings.Repeat("6f1d3a2b", 8)
	for rev, want := range map[Rev]bool{
		Rev(sha1Hex): true, Rev(sha256Hex): true,
		"": false, "main": false, "refs/heads/main": false, "6f1d3a2": false, RevHead: false,
		Rev(strings.ToUpper(sha1Hex)): false, Rev(sha1Hex + "b"): false,
		Rev(sha1Hex[:39] + "g"): false, Rev(strings.Repeat("6", 48)): false,
	} {
		assert.Equal(t, want, rev.IsObjectID(), "%q: only 40 or 64 characters of lowercase hex identify one commit forever", rev)
	}
}

func TestArchiveResponseHeaders(t *testing.T) {
	upstream := http.Header{"Etag": {`"6f1d3a2b"`}, "Content-Length": {"4096"}, "Cache-Control": {"no-cache"}}

	got := http.Header{"Etag": {`"limpet"`, `"conch"`}}
	ForwardHeaders(got, upstream, "Etag", "Content-Length", "Link")
	assert.Equal(t, http.Header{"Etag": {`"6f1d3a2b"`}, "Content-Length": {"4096"}}, got,
		"ForwardHeaders overwrites Etag, leaves the unlisted Cache-Control alone, and skips the Link that the knot never sent")

	revalidation := http.Header{}
	ForwardHeaders(revalidation, upstream, "Etag")
	assert.Equal(t, http.Header{"Etag": {`"6f1d3a2b"`}}, revalidation, "the 304 path forwards the validator without a length")

	validators := http.Header{}
	ForwardHeaders(validators, http.Header{"If-None-Match": {`"kelp"`, `"uni"`}}, "if-none-match")
	assert.Equal(t, []string{`"kelp"`, `"uni"`}, validators.Values("If-None-Match"),
		"ForwardHeaders canonicalizes a lowercase key and sends every validator that a client offered")

	untouched := httptest.NewRecorder()
	ArchiveParams{Rev: "main", Format: ArchiveTarGz}.Serve("squid").SetHeaders(untouched.Header())
	untouched.Header().Set("Content-Length", "4096")
	untouched.Header().Set("Etag", `"6f1d3a2b"`)
	untouched.Header().Set("Link", ImmutableLink(testArchiveEndpoint+"?ref=6f1d3a2b"))
	untouched.Header().Set("Cache-Control", "public, max-age=31536000")
	NewResponseBody(untouched).Fail()
	assert.Equal(t, http.StatusInternalServerError, untouched.Code)
	assert.Equal(t, http.Header{"Cache-Control": {"no-store"}, "X-Content-Type-Options": {"nosniff"}}, untouched.Header(),
		"Fail deletes the length, the type, the filename, the validator and the link, because a 500 doesn't describe the archive they came from")

	body := NewResponseBody(httptest.NewRecorder())
	written, err := body.Write([]byte("PK\x03\x04"))
	require.NoError(t, err)
	assert.Equal(t, 4, written)
	assert.PanicsWithError(t, http.ErrAbortHandler.Error(), body.Fail,
		"a truncated body has to abort the connection, since a clean return reads as a complete archive")

	archive := ArchiveParams{Rev: "6f1d3a2b", Format: ArchiveTarGz}.Serve("squid")
	etag := archive.ETag("did:plc:limpet")
	assert.Regexp(t, `^"[0-9a-f]{64}"$`, etag)
	assert.NotContains(t, []string{
		archive.ETag("did:plc:conch"),
		archive.WithRev("main").ETag("did:plc:limpet"),
		ArchiveParams{Rev: "6f1d3a2b", Format: ArchiveZip}.Serve("squid").ETag("did:plc:limpet"),
		ArchiveParams{Rev: "6f1d3a2b", Format: ArchiveTarGz}.Serve("kelp").ETag("did:plc:limpet"),
	}, etag, "a repo, rev, format or prefix that shapes different bytes gets a different validator")

	for offered, want := range map[string]bool{
		etag: true, "W/" + etag: true, "*": true, `"conch", ` + etag: true, `"conch"`: false, "": false,
	} {
		request := httptest.NewRequest(http.MethodGet, testArchiveEndpoint, nil)
		if offered != "" {
			request.Header.Set("If-None-Match", offered)
		}
		recorder := httptest.NewRecorder()
		assert.Equal(t, want, archive.ServeNotModified(recorder, request, "did:plc:limpet"), "If-None-Match %q", offered)
		assert.Equal(t, lo.Ternary(want, http.StatusNotModified, http.StatusOK), recorder.Code)
		assert.Equal(t, etag, recorder.Header().Get("Etag"), "the knot sets the validator on every answer, so a client with a stale copy learns it too")
		assert.Equal(t, "no-cache", recorder.Header().Get("Cache-Control"))
	}
}

func TestWriteArchive(t *testing.T) {
	repoPath := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(repoPath, "README.md"), []byte("# squid\n"), 0644))
	for _, args := range [][]string{
		{"init", "-q", "-b", "main"},
		{"add", "README.md"},
		{"-c", "user.name=nel", "-c", "user.email=noreply@nel.pet", "commit", "-qm", "Initial commit"},
		{"branch", "feat/uni"},
	} {
		cmd := exec.Command("git", args...)
		cmd.Dir = repoPath
		require.NoError(t, cmd.Run(), "git %v", args)
	}

	canceled, cancel := context.WithCancel(context.Background())
	cancel()

	head := ArchiveParams{Rev: RevHead, Format: ArchiveZip}
	cases := []struct {
		name    string
		ctx     context.Context
		archive ServedArchive
		want    []string
	}{
		{"the defaulted prefix begins every entry", context.Background(), ArchiveParams{Rev: "refs/heads/feat/uni", Format: ArchiveZip}.Serve("squid"), []string{"squid-feat-uni/", "squid-feat-uni/README.md"}},
		{"a requested prefix replaces the default", context.Background(), ArchiveParams{Rev: RevHead, Format: ArchiveZip, Prefix: "kelp"}.Serve("squid"), []string{"kelp/", "kelp/README.md"}},
		{"canceled context", canceled, head.Serve("squid"), nil},
		{"a ref the repo doesn't have", context.Background(), ArchiveParams{Rev: "refs/heads/limpet", Format: ArchiveZip}.Serve("squid"), nil},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			var body bytes.Buffer
			err := WriteArchive(tc.ctx, &body, repoPath, tc.archive)
			if tc.want == nil {
				assert.Error(t, err)
				return
			}
			require.NoError(t, err)

			entries, err := zip.NewReader(bytes.NewReader(body.Bytes()), int64(body.Len()))
			require.NoError(t, err)
			assert.Equal(t, tc.want, lo.Map(entries.File, func(f *zip.File, _ int) string { return f.Name }))
		})
	}

	err := WriteArchive(context.Background(), refusingWriter{}, repoPath, head.Serve("squid"))
	assert.ErrorIs(t, err, errClientGone, "WriteArchive returns the write error itself")
}

var errClientGone = errors.New("the client stopped reading")

type refusingWriter struct{}

func (refusingWriter) Write(p []byte) (int, error) { return 0, errClientGone }
