package repoverify

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hostutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
	"tangled.org/core/xrpc/xrpcclient"
)

type Result struct {
	RepoDid  repoident.RepoDid
	OwnerDid repoident.OwnerDid
	KnotURL  repoident.KnotURL
	// Rkey of the sh.tangled.repo record tracked by the knot; empty when the
	// knot does not support describeRepo.
	Rkey syntax.RecordKey
}

var ErrKnotAnswer = errors.New("knot returned an invalid describeRepo answer")

func Describe(ctx context.Context, httpClient *http.Client, knot repoident.KnotURL, repoDid repoident.RepoDid) (Result, error) {
	client := &indigoxrpc.Client{Host: knot.String(), Client: httpClient}
	out, err := tangled.RepoDescribeRepo(ctx, client, repoDid.String())
	if xrpcErr := xrpcclient.HandleXrpcErr(err); xrpcErr != nil {
		return Result{}, fmt.Errorf("describeRepo on %s: %w (%v)", knot, xrpcErr, err)
	}
	if out.RepoDid != repoDid.String() {
		return Result{}, fmt.Errorf("%w: knot %s returned repoDid %q, want %q", ErrKnotAnswer, knot, out.RepoDid, repoDid)
	}
	ownerDid, err := repoident.NewOwnerDid(out.OwnerDid)
	if err != nil {
		return Result{}, fmt.Errorf("%w from knot %s: %w", ErrKnotAnswer, knot, err)
	}
	rkey, err := syntax.ParseRecordKey(out.Rkey)
	if err != nil {
		return Result{}, fmt.Errorf("%w: knot %s returned rkey %q: %w", ErrKnotAnswer, knot, out.Rkey, err)
	}
	return Result{RepoDid: repoDid, OwnerDid: ownerDid, KnotURL: knot, Rkey: rkey}, nil
}

type Verifier func(ctx context.Context, repoDid repoident.RepoDid) (Result, error)

const verifyTimeout = 10 * time.Second

func New(resolver *idresolver.Resolver, dev bool) Verifier {
	httpClient := hostutil.SafeClient(dev, verifyTimeout)
	policy := repoident.SchemeFor(dev)

	return func(ctx context.Context, repoDid repoident.RepoDid) (Result, error) {
		ctx, cancel := context.WithTimeout(ctx, verifyTimeout)
		defer cancel()

		ident, err := resolver.ResolveIdent(ctx, repoDid.String())
		if err != nil {
			return Result{}, fmt.Errorf("resolve repoDid %s: %w", repoDid, err)
		}

		knot, err := repoident.KnotURLFromIdentity(ident, policy)
		if err != nil {
			return Result{}, fmt.Errorf("repoDid %s: %w", repoDid, err)
		}

		result, err := Describe(ctx, httpClient, knot, repoDid)
		if errors.Is(err, xrpcclient.ErrXrpcUnsupported) {
			return Result{RepoDid: repoDid, KnotURL: knot}, nil
		}
		return result, err
	}
}
