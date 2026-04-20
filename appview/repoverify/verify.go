package repoverify

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"syscall"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/idresolver"
)

type RepoDid syntax.DID

func (r RepoDid) String() string { return string(r) }

func NewRepoDid(s string) (RepoDid, error) {
	did, err := syntax.ParseDID(s)
	if err != nil {
		return "", fmt.Errorf("invalid repoDid %q: %w", s, err)
	}
	return RepoDid(did), nil
}

type OwnerDid syntax.DID

func (o OwnerDid) String() string { return string(o) }

func NewOwnerDid(s string) (OwnerDid, error) {
	did, err := syntax.ParseDID(s)
	if err != nil {
		return "", fmt.Errorf("invalid ownerDid %q: %w", s, err)
	}
	return OwnerDid(did), nil
}

func ParseKnotEndpoint(raw string, dev bool) (*url.URL, error) {
	if raw == "" {
		return nil, fmt.Errorf("empty knot URL")
	}
	u, err := url.Parse(raw)
	if err != nil {
		return nil, fmt.Errorf("invalid knot URL %q: %w", raw, err)
	}
	if u.Host == "" {
		return nil, fmt.Errorf("knot URL %q has no host", raw)
	}
	switch u.Scheme {
	case "https":
	case "http":
		if !dev {
			return nil, fmt.Errorf("knot URL %q must use https outside dev mode", raw)
		}
	default:
		return nil, fmt.Errorf("knot URL %q has unsupported scheme %q", raw, u.Scheme)
	}
	return u, nil
}

type Result struct {
	RepoDid  RepoDid
	OwnerDid OwnerDid
	KnotURL  *url.URL
}

type Verifier func(ctx context.Context, repoDid RepoDid) (Result, error)

const verifyTimeout = 10 * time.Second

func New(resolver *idresolver.Resolver, dev bool) Verifier {
	transport := &http.Transport{
		DialContext: safeDialer(dev).DialContext,
	}
	httpClient := &http.Client{
		Timeout:   verifyTimeout,
		Transport: transport,
	}

	return func(ctx context.Context, repoDid RepoDid) (Result, error) {
		ctx, cancel := context.WithTimeout(ctx, verifyTimeout)
		defer cancel()
		return resolveAndDescribe(ctx, resolver, httpClient, repoDid, dev)
	}
}

func resolveAndDescribe(
	ctx context.Context,
	resolver *idresolver.Resolver,
	httpClient *http.Client,
	repoDid RepoDid,
	dev bool,
) (Result, error) {
	ident, err := resolver.ResolveIdent(ctx, repoDid.String())
	if err != nil {
		return Result{}, fmt.Errorf("resolve repoDid %s: %w", repoDid, err)
	}

	knot, err := ParseKnotEndpoint(ident.GetServiceEndpoint("atproto_pds"), dev)
	if err != nil {
		return Result{}, fmt.Errorf("repoDid %s: %w", repoDid, err)
	}

	client := &indigoxrpc.Client{Host: knot.String(), Client: httpClient}
	out, err := tangled.RepoDescribeRepo(ctx, client, repoDid.String())
	if xrpcErr := xrpcclient.HandleXrpcErr(err); xrpcErr != nil {
		return Result{}, fmt.Errorf("describeRepo on %s: %w", knot, xrpcErr)
	}

	if out.RepoDid != repoDid.String() {
		return Result{}, fmt.Errorf("knot %s returned mismatched repoDid: got %q, want %q", knot, out.RepoDid, repoDid)
	}

	ownerDid, err := NewOwnerDid(out.OwnerDid)
	if err != nil {
		return Result{}, fmt.Errorf("describeRepo on %s returned invalid ownerDid: %w", knot, err)
	}

	return Result{
		RepoDid:  repoDid,
		OwnerDid: ownerDid,
		KnotURL:  knot,
	}, nil
}

func safeDialer(dev bool) *net.Dialer {
	d := &net.Dialer{
		Timeout:   5 * time.Second,
		KeepAlive: 30 * time.Second,
	}
	if dev {
		return d
	}
	d.Control = func(network, address string, _ syscall.RawConn) error {
		host, _, err := net.SplitHostPort(address)
		if err != nil {
			return fmt.Errorf("invalid dial address %q: %w", address, err)
		}
		ip := net.ParseIP(host)
		if ip == nil {
			return fmt.Errorf("dial address %q did not resolve to IP", address)
		}
		if ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast() ||
			ip.IsLinkLocalMulticast() || ip.IsMulticast() || ip.IsUnspecified() {
			return fmt.Errorf("refusing to dial %s: reserved or private address", ip)
		}
		return nil
	}
	return d
}
