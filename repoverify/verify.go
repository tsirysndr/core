package repoverify

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"syscall"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
	"tangled.org/core/xrpc/xrpcclient"
)

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
	RepoDid  repoident.RepoDid
	OwnerDid repoident.OwnerDid
	KnotURL  *url.URL
	// Rkey of the sh.tangled.repo record tracked by the knot; empty when the
	// knot does not support describeRepo.
	Rkey string
}

type Verifier func(ctx context.Context, repoDid repoident.RepoDid) (Result, error)

const verifyTimeout = 10 * time.Second

func New(resolver *idresolver.Resolver, dev bool) Verifier {
	transport := &http.Transport{
		DialContext: safeDialer(dev).DialContext,
	}
	httpClient := &http.Client{
		Timeout:   verifyTimeout,
		Transport: transport,
	}

	return func(ctx context.Context, repoDid repoident.RepoDid) (Result, error) {
		ctx, cancel := context.WithTimeout(ctx, verifyTimeout)
		defer cancel()
		return resolveAndDescribe(ctx, resolver, httpClient, repoDid, dev)
	}
}

func resolveAndDescribe(
	ctx context.Context,
	resolver *idresolver.Resolver,
	httpClient *http.Client,
	repoDid repoident.RepoDid,
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
		if errors.Is(xrpcErr, xrpcclient.ErrXrpcUnsupported) {
			return Result{RepoDid: repoDid, KnotURL: knot}, nil
		}
		return Result{}, fmt.Errorf("describeRepo on %s: %w", knot, xrpcErr)
	}

	if out.RepoDid != repoDid.String() {
		return Result{}, fmt.Errorf("knot %s returned mismatched repoDid: got %q, want %q", knot, out.RepoDid, repoDid)
	}

	ownerDid, err := repoident.NewOwnerDid(out.OwnerDid)
	if err != nil {
		return Result{}, fmt.Errorf("describeRepo on %s returned invalid ownerDid: %w", knot, err)
	}

	return Result{
		RepoDid:  repoDid,
		OwnerDid: ownerDid,
		KnotURL:  knot,
		Rkey:     out.Rkey,
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
