package repo

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"slices"
	"strings"
	"testing"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/models"
	"tangled.org/core/gitutil"
)

const (
	testRepoDid   = "did:plc:limpet"
	testRepoOwner = "did:plc:boltless"
	testRepoRkey  = "3kzabcdefghij"
	testRepoPath  = "/boltless.dev/squid"
)

func driveArchiveRoute(t *testing.T, target, userAgent string) (archiveRequest, error) {
	t.Helper()

	var (
		request archiveRequest
		err     error
	)
	router := chi.NewRouter()
	router.Get("/{user}/{repo}"+archiveRoute, func(w http.ResponseWriter, r *http.Request) {
		request, err = parseArchiveRequest(r)
	})

	if parsed, parseErr := url.Parse(target); parseErr == nil && parsed.Host != "" {
		target = parsed.RequestURI()
	}
	if rest, isRepoDid := strings.CutPrefix(target, "/"+testRepoDid); isRepoDid {
		target = "/" + testRepoOwner + "/" + testRepoRkey + rest
	}

	req := httptest.NewRequest(http.MethodGet, target, nil)
	req.Header.Set("User-Agent", userAgent)
	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("%s: status = %d, want the archive route to match", target, rec.Code)
	}
	return request, err
}

func TestImmutableArchiveURL(t *testing.T) {
	rp := &Repo{config: &config.Config{Core: config.CoreConfig{Dev: true, AppviewHost: "tangled.org"}}}
	repo := &models.Repo{Did: testRepoOwner, Rkey: testRepoRkey, Name: "squid", RepoDid: testRepoDid}
	rev := gitutil.Rev("6f1d3a2b4c5d6e7f8091a2b3c4d5e6f708192a3b")
	targz, zip := gitutil.ArchiveTarGz, gitutil.ArchiveZip
	base := "http://tangled.org/" + testRepoDid + "/archive/" + rev.String()

	for _, tc := range []struct {
		params gitutil.ArchiveParams
		want   string
	}{
		{gitutil.ArchiveParams{Rev: rev, Format: zip, Prefix: "did:plc:boltless"}, base + ".zip?prefix=did%3Aplc%3Aboltless"},
		{gitutil.ArchiveParams{Rev: "refs/heads/feat/uni", Format: targz}, base + ".tar.gz?prefix=squid-feat-uni"},
		{gitutil.ArchiveParams{Rev: rev, Format: targz, Prefix: "kelp&format=zip#uni"}, base + ".tar.gz?prefix=kelp%26format%3Dzip%23uni"},
	} {
		archive := tc.params.Serve("squid").WithRev(rev)
		got := rp.immutableArchiveURL(repo, archive)
		if got != tc.want {
			t.Fatalf("immutable URL = %q, want %q, where the URL spells out the prefix that the knot will serve, percent-escaped", got, tc.want)
		}

		request, err := driveArchiveRoute(t, got, "")
		if err != nil || request.params.Serve("squid") != archive || request.guessed {
			t.Fatalf("parsing %q gave %+v (guessed %v, err %v), want the archive %+v off the suffix", got, request.params, request.guessed, err, archive)
		}
		if again := rp.immutableArchiveURL(repo, request.params.Serve("squid")); again != got {
			t.Errorf("rebuilding gave %q, want an immutable URL that is its own fixed point at %q", again, got)
		}
	}
}

func TestParseArchiveRequest(t *testing.T) {
	windows := "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
	targz, zip := gitutil.ArchiveTarGz, gitutil.ArchiveZip
	cases := []struct {
		name        string
		path        string
		userAgent   string
		want        gitutil.ArchiveParams
		wantGuessed bool
		wantErr     bool
	}{
		{"short ref", testRepoPath + "/archive/v1.0.0?format=tar.gz", "", gitutil.ArchiveParams{Rev: "v1.0.0", Format: targz}, false, false},
		{"full ref unescaped", testRepoPath + "/archive/refs/tags/v1.0.0?prefix=did:plc:boltless", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: targz, Prefix: "did:plc:boltless"}, true, false},
		{"full ref escaped", testRepoPath + "/archive/refs%2Ftags%2Fv1.0.0?prefix=did:plc:boltless", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: targz, Prefix: "did:plc:boltless"}, true, false},
		{"format from suffix", testRepoPath + "/archive/refs/tags/v1.0.0.zip", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: zip}, false, false},
		{"escaped ref with a format suffix", testRepoPath + "/archive/refs%2Ftags%2Fv1.0.0.tar.gz", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: targz}, false, false},
		{"a tag ending in a format suffix", testRepoPath + "/archive/refs%2Ftags%2Fv1.0.0.zip.tar.gz", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0.zip", Format: targz}, false, false},
		{"unknown format query with a zip suffix", testRepoPath + "/archive/refs/tags/v1.0.0.zip?format=tar.xz", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: zip}, false, false},
		{"zip for a windows user agent", testRepoPath + "/archive/main?format=tar.xz", windows, gitutil.ArchiveParams{Rev: "main", Format: zip}, true, false},
		{"percent in the ref itself", testRepoPath + "/archive/refs/tags/a%252Fb", "", gitutil.ArchiveParams{Rev: "refs/tags/a%2Fb", Format: targz}, true, false},
		{"prefix wrapped in slashes", testRepoPath + "/archive/main?prefix=/kelp/", "", gitutil.ArchiveParams{Rev: "main", Format: targz, Prefix: "kelp"}, true, false},
		{"traversal escaped", testRepoPath + "/archive/..%2F..%2Fetc", "", gitutil.ArchiveParams{Rev: "../../etc", Format: targz}, true, false},
		{"parse deletes a ref query", testRepoPath + "/archive/main?ref=other", "", gitutil.ArchiveParams{Rev: "main", Format: targz}, true, false},

		{"empty ref", testRepoPath + "/archive/", "", gitutil.ArchiveParams{}, false, true},
		{"escaped space", testRepoPath + "/archive/refs/tags/a%20b", "", gitutil.ArchiveParams{}, false, true},
		{"ref that git would read as an option", testRepoPath + "/archive/--output=%2Ftmp%2Fevil", "", gitutil.ArchiveParams{}, false, true},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, gotErr := driveArchiveRoute(t, tc.path, tc.userAgent)
			if got.params != tc.want || (gotErr != nil) != tc.wantErr {
				t.Fatalf("params = %+v with err %v, want %+v and rejected = %v", got.params, gotErr, tc.want, tc.wantErr)
			}
			if got.guessed != tc.wantGuessed {
				t.Errorf("format guessed from the user agent = %v, want %v", got.guessed, tc.wantGuessed)
			}
		})
	}
}

func TestSetArchiveCache(t *testing.T) {
	hash := gitutil.Rev("6f1d3a2b4c5d6e7f8091a2b3c4d5e6f708192a3b")
	sha256Hash := gitutil.Rev(strings.Repeat("6f1d3a2b", 8))
	withPrefix := func(rev gitutil.Rev) gitutil.ArchiveParams {
		return gitutil.ArchiveParams{Rev: rev, Format: gitutil.ArchiveTarGz, Prefix: "squid-main"}
	}

	for _, tc := range []struct {
		name     string
		request  archiveRequest
		resolved gitutil.Rev
		want     string
	}{
		{"a commit and an explicit prefix in the URL earn a year", archiveRequest{params: withPrefix(hash)}, hash, immutableArchiveCache},
		{"a sha256 URL addresses a commit 64 characters wide", archiveRequest{params: withPrefix(sha256Hash)}, sha256Hash, immutableArchiveCache},
		{"a defaulted prefix comes from the repo name, which a rename changes", archiveRequest{params: gitutil.ArchiveParams{Rev: hash, Format: gitutil.ArchiveTarGz}}, hash, "no-cache"},
		{"a URL with a branch in it doesn't address one commit, since the branch moves", archiveRequest{params: withPrefix("main")}, hash, "no-cache"},
		{"a knot that doesn't advertise an immutable link leaves the commit unresolved", archiveRequest{params: withPrefix(hash)}, "", "no-cache"},
		{"a knot echoing a branch back as immutable doesn't earn that URL a year", archiveRequest{params: withPrefix("main")}, "main", "no-cache"},
	} {
		got := http.Header{}
		setArchiveCache(got, tc.request, tc.resolved)
		if control, vary := got.Get("Cache-Control"), got.Values("Vary"); control != tc.want || len(vary) > 0 {
			t.Errorf("%s: Cache-Control = %q varying on %v, want %q without a Vary", tc.name, control, vary, tc.want)
		}
	}

	guessed := http.Header{"Vary": {"Accept-Encoding"}}
	setArchiveCache(guessed, archiveRequest{params: withPrefix(hash), guessed: true}, hash)
	if want := []string{"Accept-Encoding", "User-Agent"}; guessed.Get("Cache-Control") != "no-cache" || !slices.Equal(guessed.Values("Vary"), want) {
		t.Errorf("a user-agent guess gave %q varying on %v, want no-cache and %v, since a guessed format varies the bytes with the user agent",
			guessed.Get("Cache-Control"), guessed.Values("Vary"), want)
	}
}
