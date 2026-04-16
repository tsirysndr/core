package idresolver

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/identity/redisdir"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/carlmjohnson/versioninfo"
)

type Resolver struct {
	directory identity.Directory
	base      *identity.BaseDirectory
}

func BaseDirectory(plcUrl string) *identity.BaseDirectory {
	base := identity.BaseDirectory{
		PLCURL: plcUrl,
		HTTPClient: http.Client{
			Timeout: time.Second * 10,
			Transport: &http.Transport{
				// would want this around 100ms for services doing lots of handle resolution. Impacts PLC connections as well, but not too bad.
				IdleConnTimeout: time.Millisecond * 1000,
				MaxIdleConns:    100,
			},
		},
		Resolver: net.Resolver{
			Dial: func(ctx context.Context, network, address string) (net.Conn, error) {
				d := net.Dialer{Timeout: time.Second * 3}
				return d.DialContext(ctx, network, address)
			},
		},
		TryAuthoritativeDNS: true,
		// primary Bluesky PDS instance only supports HTTP resolution method
		SkipDNSDomainSuffixes: []string{".bsky.social"},
		UserAgent:             "indigo-identity/" + versioninfo.Short(),
	}
	return &base
}

func DefaultResolver(plcUrl string) *Resolver {
	base := BaseDirectory(plcUrl)
	cached := identity.NewCacheDirectory(base, 250_000, time.Hour*24, time.Minute*2, time.Minute*5)
	return &Resolver{
		directory: cached,
		base:      base,
	}
}

func RedisDirectory(base *identity.BaseDirectory, url string) (identity.Directory, error) {
	hitTTL := time.Hour * 24
	errTTL := time.Second * 30
	invalidHandleTTL := time.Minute * 5
	return redisdir.NewRedisDirectory(base, url, hitTTL, errTTL, invalidHandleTTL, 10000)
}

func RedisResolver(redisUrl, plcUrl string) (*Resolver, error) {
	base := BaseDirectory(plcUrl)
	directory, err := RedisDirectory(base, redisUrl)
	if err != nil {
		return nil, err
	}
	return &Resolver{
		directory: directory,
		base:      base,
	}, nil
}

type handleResolver interface {
	ResolveHandle(ctx context.Context, h syntax.Handle) (syntax.DID, error)
}

func (r *Resolver) ResolveHandle(ctx context.Context, handle syntax.Handle) (syntax.DID, error) {
	if hr, ok := r.directory.(handleResolver); ok {
		return hr.ResolveHandle(ctx, handle)
	}
	return r.base.ResolveHandle(ctx, handle)
}

func (r *Resolver) ResolveAtIdentifier(ctx context.Context, input string) (*identity.Identity, error) {
	if did, err := syntax.ParseDID(input); err == nil {
		return r.directory.LookupDID(ctx, did)
	}
	handle, err := syntax.ParseHandle(input)
	if err != nil {
		return nil, fmt.Errorf("not a did or handle: %w", err)
	}
	handle = handle.Normalize()
	did, err := r.base.ResolveHandle(ctx, handle)
	if err != nil {
		return nil, fmt.Errorf("resolve handle %q: %w", handle, err)
	}
	ident, err := r.directory.LookupDID(ctx, did)
	if err != nil {
		return nil, fmt.Errorf("lookup did for %q: %w", handle, err)
	}
	aka := "at://" + handle.String()
	if !slices.ContainsFunc(ident.AlsoKnownAs, func(s string) bool { return strings.EqualFold(s, aka) }) {
		return nil, fmt.Errorf("handle %q not declared in alsoKnownAs for %s", handle, did)
	}
	return ident, nil
}

func (r *Resolver) ResolveIdent(ctx context.Context, arg string) (*identity.Identity, error) {
	id, err := syntax.ParseAtIdentifier(arg)
	if err != nil {
		return nil, err
	}

	return r.directory.Lookup(ctx, id)
}

func (r *Resolver) ResolveIdents(ctx context.Context, idents []string) []*identity.Identity {
	results := make([]*identity.Identity, len(idents))
	var wg sync.WaitGroup

	done := make(chan struct{})
	defer close(done)

	for idx, ident := range idents {
		wg.Add(1)
		go func(index int, id string) {
			defer wg.Done()

			select {
			case <-ctx.Done():
				results[index] = nil
			case <-done:
				results[index] = nil
			default:
				identity, _ := r.ResolveIdent(ctx, id)
				results[index] = identity
			}
		}(idx, ident)
	}

	wg.Wait()
	return results
}

func (r *Resolver) InvalidateIdent(ctx context.Context, arg string) error {
	id, err := syntax.ParseAtIdentifier(arg)
	if err != nil {
		return err
	}

	return r.directory.Purge(ctx, id)
}

func (r *Resolver) Directory() identity.Directory {
	return r.directory
}
