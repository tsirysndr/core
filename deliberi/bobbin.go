package deliberi

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
)

type recipientResolver interface {
	ListRecipients(ctx context.Context, uri string) ([]string, error)
	RepoOwner(ctx context.Context, repoDid string) (ownerDid, name string, err error)
}

type bobbinClient struct {
	xc *indigoxrpc.Client
}

func newBobbinClient(apiUrl string) *bobbinClient {
	return &bobbinClient{
		xc: &indigoxrpc.Client{
			Host:   strings.TrimRight(apiUrl, "/"),
			Client: &http.Client{Timeout: 10 * time.Second},
		},
	}
}

func (c *bobbinClient) ListRecipients(ctx context.Context, uri string) ([]string, error) {
	out, err := tangled.TempNotificationListRecipients(ctx, c.xc, uri)
	if err != nil {
		return nil, fmt.Errorf("calling %s: %w", tangled.TempNotificationListRecipientsNSID, err)
	}
	return out.Dids, nil
}

// repoByRepoDid mirrors sh.tangled.repo.getRepoByRepoDid's output, but keeps the
// record raw: the owner comes from the uri's authority, so a record we can't
// type-decode still resolves an owner.
type repoByRepoDid struct {
	Uri   string          `json:"uri"`
	Value json.RawMessage `json:"value"`
}

// RepoOwner resolves a repo DID to the did that holds its sh.tangled.repo
// record, along with the repo's cosmetic name.
func (c *bobbinClient) RepoOwner(ctx context.Context, repoDid string) (string, string, error) {
	var out repoByRepoDid
	params := map[string]any{"repoDid": repoDid}
	if err := c.xc.Do(ctx, indigoxrpc.Query, "", tangled.RepoGetRepoByRepoDidNSID, params, nil, &out); err != nil {
		return "", "", fmt.Errorf("calling %s: %w", tangled.RepoGetRepoByRepoDidNSID, err)
	}

	owner := syntax.ATURI(out.Uri).Authority().String()
	if owner == "" {
		return "", "", fmt.Errorf("no authority in uri %q", out.Uri)
	}

	// the name is cosmetic, so a decode failure is not fatal.
	var rec struct {
		Name string `json:"name"`
	}
	_ = json.Unmarshal(out.Value, &rec)

	return owner, rec.Name, nil
}
