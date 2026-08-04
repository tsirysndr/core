package repo

import (
	"net/http"
	"net/http/httptest"
	"net/url"
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

func TestParseArchiveRequest(t *testing.T) {
	var (
		got    gitutil.ArchiveParams
		gotErr error
	)
	router := chi.NewRouter()
	router.Get("/{user}/{repo}"+archiveRoute, func(w http.ResponseWriter, r *http.Request) {
		got, gotErr = parseArchiveRequest(r)
	})

	resolvedParams := gitutil.ArchiveParams{
		Rev:    "6f1d3a2b4c5d6e7f8091a2b3c4d5e6f708192a3b",
		Format: gitutil.ArchiveZip,
		Prefix: gitutil.ArchivePrefix("").OrDefault("squid", "refs/heads/feat/uni"),
	}
	rp := &Repo{config: &config.Config{Core: config.CoreConfig{Dev: true, AppviewHost: "tangled.org"}}}
	immutable, err := url.Parse(rp.immutableArchiveURL(
		&models.Repo{Did: testRepoOwner, Rkey: testRepoRkey, Name: "squid", RepoDid: testRepoDid},
		resolvedParams,
	))
	if err != nil {
		t.Fatalf("the immutable URL must parse: %v", err)
	}

	windows := "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
	targz, zip := gitutil.ArchiveTarGz, gitutil.ArchiveZip
	cases := []struct {
		name      string
		path      string
		userAgent string
		want      gitutil.ArchiveParams
		wantErr   bool
	}{
		{"short ref", testRepoPath + "/archive/v1.0.0?format=tar.gz", "", gitutil.ArchiveParams{Rev: "v1.0.0", Format: targz}, false},
		{"full ref unescaped", testRepoPath + "/archive/refs/tags/v1.0.0?prefix=did:plc:boltless", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: targz, Prefix: "did:plc:boltless"}, false},
		{"full ref escaped", testRepoPath + "/archive/refs%2Ftags%2Fv1.0.0?prefix=did:plc:boltless", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: targz, Prefix: "did:plc:boltless"}, false},
		{"format from suffix", testRepoPath + "/archive/refs/tags/v1.0.0.zip", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: zip}, false},
		{"unknown format query with a zip suffix", testRepoPath + "/archive/refs/tags/v1.0.0.zip?format=tar.xz", "", gitutil.ArchiveParams{Rev: "refs/tags/v1.0.0", Format: zip}, false},
		{"zip for a windows user agent", testRepoPath + "/archive/main?format=tar.xz", windows, gitutil.ArchiveParams{Rev: "main", Format: zip}, false},
		{"percent in the ref itself", testRepoPath + "/archive/refs/tags/a%252Fb", "", gitutil.ArchiveParams{Rev: "refs/tags/a%2Fb", Format: targz}, false},
		{"prefix wrapped in slashes", testRepoPath + "/archive/main?prefix=/kelp/", "", gitutil.ArchiveParams{Rev: "main", Format: targz, Prefix: "kelp"}, false},
		{"traversal escaped", testRepoPath + "/archive/..%2F..%2Fetc", "", gitutil.ArchiveParams{Rev: "../../etc", Format: targz}, false},
		{"parse deletes a ref query", testRepoPath + "/archive/main?ref=other", "", gitutil.ArchiveParams{Rev: "main", Format: targz}, false},
		{"our own immutable URL", immutable.RequestURI(), "", resolvedParams, false},

		{"empty ref", testRepoPath + "/archive/", "", gitutil.ArchiveParams{}, true},
		{"escaped space", testRepoPath + "/archive/refs/tags/a%20b", "", gitutil.ArchiveParams{}, true},
		{"ref that git would read as an option", testRepoPath + "/archive/--output=%2Ftmp%2Fevil", "", gitutil.ArchiveParams{}, true},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, gotErr = gitutil.ArchiveParams{}, nil
			path := tc.path
			if rest, isRepoDid := strings.CutPrefix(path, "/"+testRepoDid); isRepoDid {
				path = "/" + testRepoOwner + "/" + testRepoRkey + rest
			}

			req := httptest.NewRequest(http.MethodGet, path, nil)
			req.Header.Set("User-Agent", tc.userAgent)
			rec := httptest.NewRecorder()
			router.ServeHTTP(rec, req)

			if rec.Code != http.StatusOK {
				t.Fatalf("%s: status = %d, want the archive route to match", path, rec.Code)
			}
			if got != tc.want || (gotErr != nil) != tc.wantErr {
				t.Errorf("params = %+v with err %v, want %+v and rejected = %v", got, gotErr, tc.want, tc.wantErr)
			}
		})
	}
}
