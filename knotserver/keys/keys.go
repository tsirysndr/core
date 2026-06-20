package keys

import (
	"context"
	"fmt"
	"log/slog"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/log"
)

const (
	publicKeyPageSize = 100
	maxPublicKeyPages = 20
)

func FetchAndStore(ctx context.Context, dir identity.Directory, store *db.DB, did syntax.DID) error {
	l := log.FromContext(ctx)

	id, err := dir.LookupDID(ctx, did)
	if err != nil {
		return fmt.Errorf("lookup did to fetch keys: %w", err)
	}

	serviceEndpoint, ok := id.Services["atproto_pds"]
	if !ok {
		l.Warn("did identity did not contain atproto_pds service while adding their keys", "did", did)
		return nil
	}

	xrpcc := indigoxrpc.Client{Host: serviceEndpoint.URL}
	records, err := listAllPublicKeys(ctx, l, &xrpcc, did, "", maxPublicKeyPages)
	if err != nil {
		return fmt.Errorf("fetching public keys for did: %w", err)
	}

	keys := collectPublicKeys(l, did, records)
	if len(keys) == 0 {
		l.Warn("no public keys fetched, skipping replace so existing keys are not wiped by a transient empty response", "did", did)
		return nil
	}

	if err := store.ReplacePublicKeys(did, keys); err != nil {
		return fmt.Errorf("replacing public keys in db: %w", err)
	}
	return nil
}

func listAllPublicKeys(ctx context.Context, l *slog.Logger, xrpcc *indigoxrpc.Client, did syntax.DID, cursor string, pagesLeft int) ([]*comatproto.RepoListRecords_Record, error) {
	if pagesLeft <= 0 {
		l.Warn("public key pagination hit page cap, remaining keys ignored", "did", did, "cap", maxPublicKeyPages)
		return nil, nil
	}

	resp, err := comatproto.RepoListRecords(ctx, xrpcc, tangled.PublicKeyNSID, cursor, publicKeyPageSize, did.String(), false)
	if err != nil {
		return nil, err
	}

	if resp.Cursor == nil || *resp.Cursor == "" || len(resp.Records) == 0 {
		return resp.Records, nil
	}

	rest, err := listAllPublicKeys(ctx, l, xrpcc, did, *resp.Cursor, pagesLeft-1)
	if err != nil {
		return nil, err
	}

	return append(resp.Records, rest...), nil
}

func collectPublicKeys(l *slog.Logger, did syntax.DID, records []*comatproto.RepoListRecords_Record) []db.PublicKey {
	return collectPublicKeysInto(l, did, records, nil)
}

func collectPublicKeysInto(l *slog.Logger, did syntax.DID, records []*comatproto.RepoListRecords_Record, acc []db.PublicKey) []db.PublicKey {
	if len(records) == 0 {
		return acc
	}

	return collectPublicKeysInto(l, did, records[1:], appendValidKey(l, did, acc, records[0]))
}

func appendValidKey(l *slog.Logger, did syntax.DID, acc []db.PublicKey, record *comatproto.RepoListRecords_Record) []db.PublicKey {
	if record == nil {
		return acc
	}

	key, ok := record.Value.Val.(*tangled.PublicKey)
	if !ok || key == nil {
		return acc
	}

	rkey, err := recordKeyFromURI(record.Uri)
	if err != nil {
		l.Warn("skipping public key with unparseable uri", "uri", record.Uri, "err", err)
		return acc
	}

	return append(acc, db.PublicKey{
		Did:       did,
		Rkey:      rkey,
		PublicKey: *key,
	})
}

func recordKeyFromURI(uri string) (syntax.RecordKey, error) {
	aturi, err := syntax.ParseATURI(uri)
	if err != nil {
		return "", err
	}
	return aturi.RecordKey(), nil
}
