package knotserver

import (
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/stretchr/testify/require"
	"tangled.org/core/knotserver/db"
)

const (
	appviewURL    = "https://tangled.org/"
	user          = "willdot.net"
	userDID       = "did:plc:dadhhalkfcq3gucaq25hjqon"
	pushedBranch  = "feature-abc"
	defaultBranch = "main"
)

func TestCreatePullURL(t *testing.T) {

	tt := map[string]struct {
		repoName    string
		remote      string
		expectedURL string
	}{
		"not a fork": {
			repoName:    "knot-testing",
			remote:      "",
			expectedURL: "https://tangled.org/willdot.net/knot-testing/pulls/new?source=branch&sourceBranch=feature-abc&targetBranch=main",
		},
		"is fork": {
			repoName:    "knot-testing-fork",
			remote:      "https://knot1.tangled.sh/did:plc:dadhhalkfcq3gucaq25hjqon/knot-testing",
			expectedURL: "https://tangled.org/did:plc:dadhhalkfcq3gucaq25hjqon/knot-testing/pulls/new?fork=did%3Aplc%3Adadhhalkfcq3gucaq25hjqon%2Fknot-testing-fork&source=fork&sourceBranch=feature-abc&targetBranch=main",
		},
		"is fork on same knot": {
			repoName:    "knot-testing-fork",
			remote:      "file:///home/git/repositories/did:plc:ixran6dpypl5lslliiqceshs",
			expectedURL: "https://tangled.org/did:plc:dadhhalkfcq3gucaq25hjqon/knot-testing/pulls/new?fork=did%3Aplc%3Adadhhalkfcq3gucaq25hjqon%2Fknot-testing-fork&source=fork&sourceBranch=feature-abc&targetBranch=main",
		},
	}

	for name, tc := range tt {
		t.Run(name, func(t *testing.T) {
			database, err := db.Setup(t.Context(), ":memory:")
			require.NoError(t, err)
			err = database.StoreRepoKey("did:plc:ixran6dpypl5lslliiqceshs", []byte{}, "did:plc:dadhhalkfcq3gucaq25hjqon", "knot-testing", "at://uri")
			require.NoError(t, err)

			h := InternalHandle{
				db: database,
			}
			res, err := h.createPullURL(appviewURL, tc.remote, user, userDID, tc.repoName, pushedBranch, defaultBranch)
			require.NoError(t, err)

			assert.Equal(t, tc.expectedURL, res)
		})
	}
}
