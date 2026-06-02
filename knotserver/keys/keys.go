package keys

import (
	"context"
	"fmt"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/log"
)

func FetchAndStore(ctx context.Context, dir identity.Directory, store *db.DB, did string) error {
	l := log.FromContext(ctx)

	id, err := dir.LookupDID(ctx, syntax.DID(did))
	if err != nil {
		return fmt.Errorf("lookup did to fetch keys: %w", err)
	}

	serviceEndpoint, ok := id.Services["atproto_pds"]
	if !ok {
		l.Warn("did identity did not contain atproto_pds service while adding their keys", "did", did)
		return nil
	}

	xrpcc := indigoxrpc.Client{Host: serviceEndpoint.URL}
	resp, err := comatproto.RepoListRecords(ctx, &xrpcc, tangled.PublicKeyNSID, "", 50, did, false)
	if err != nil {
		return fmt.Errorf("fetching public keys for did: %w", err)
	}

	for _, record := range resp.Records {
		if record == nil {
			continue
		}
		key, ok := record.Value.Val.(*tangled.PublicKey)
		if !ok || key == nil {
			continue
		}
		if err := store.AddPublicKey(db.PublicKey{Did: did, PublicKey: *key}); err != nil {
			return fmt.Errorf("adding public key to db: %w", err)
		}
	}
	return nil
}
