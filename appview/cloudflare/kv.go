package cloudflare

import (
	"context"
	"fmt"
	"io"
	"strings"

	cf "github.com/cloudflare/cloudflare-go/v6"
	"github.com/cloudflare/cloudflare-go/v6/kv"
	"github.com/cloudflare/cloudflare-go/v6/shared"
)

// KVPut writes or overwrites a single Workers KV entry.
func (cl *Client) KVPut(ctx context.Context, key string, value []byte) error {
	_, err := cl.kvAPI.KV.Namespaces.Values.Update(ctx, cl.kvNS, key, kv.NamespaceValueUpdateParams{
		AccountID: cf.F(cl.cfAcct),
		Value:     cf.F[kv.NamespaceValueUpdateParamsValueUnion](shared.UnionString(value)),
	})
	if err != nil {
		return fmt.Errorf("writing KV entry %q: %w", key, err)
	}
	return nil
}

// KVGet reads a single Workers KV entry. Returns nil, nil if the key does not exist.
func (cl *Client) KVGet(ctx context.Context, key string) ([]byte, error) {
	res, err := cl.kvAPI.KV.Namespaces.Values.Get(ctx, cl.kvNS, key, kv.NamespaceValueGetParams{
		AccountID: cf.F(cl.cfAcct),
	})
	if err != nil {
		// The CF SDK returns a 404 when the key doesn't exist. The error type
		// lives in an internal package so we match on the error string instead.
		if strings.Contains(err.Error(), "404") && strings.Contains(err.Error(), "key not found") {
			return nil, nil
		}
		return nil, fmt.Errorf("reading KV entry %q: %w", key, err)
	}
	defer res.Body.Close()
	val, err := io.ReadAll(res.Body)
	if err != nil {
		return nil, fmt.Errorf("reading KV entry body %q: %w", key, err)
	}
	return val, nil
}

// KVDelete removes a single Workers KV entry.
// Safe to call even if the entry does not exist.
func (cl *Client) KVDelete(ctx context.Context, key string) error {
	_, err := cl.kvAPI.KV.Namespaces.Values.Delete(ctx, cl.kvNS, key, kv.NamespaceValueDeleteParams{
		AccountID: cf.F(cl.cfAcct),
	})
	if err != nil {
		return fmt.Errorf("deleting KV entry %q: %w", key, err)
	}
	return nil
}

// KVDeleteByPrefix removes every Workers KV entry whose key starts with prefix,
// handling pagination automatically.
func (cl *Client) KVDeleteByPrefix(ctx context.Context, prefix string) error {
	iter := cl.kvAPI.KV.Namespaces.Keys.ListAutoPaging(ctx, cl.kvNS, kv.NamespaceKeyListParams{
		AccountID: cf.F(cl.cfAcct),
		Prefix:    cf.F(prefix),
	})

	for iter.Next() {
		entry := iter.Current()
		if _, err := cl.kvAPI.KV.Namespaces.Values.Delete(ctx, cl.kvNS, entry.Name, kv.NamespaceValueDeleteParams{
			AccountID: cf.F(cl.cfAcct),
		}); err != nil {
			return fmt.Errorf("deleting KV entry %q: %w", entry.Name, err)
		}
	}
	if err := iter.Err(); err != nil {
		return fmt.Errorf("listing KV entries with prefix %q: %w", prefix, err)
	}

	return nil
}
