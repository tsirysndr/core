package repoverify

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/samber/lo"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hostutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
	"tangled.org/core/xrpc/xrpcclient"
)

type Answer string

const (
	AnswerUnset   Answer = "unset"
	AnswerOwner   Answer = "owner"
	AnswerNoRoute Answer = "noRoute"
	AnswerAbsent  Answer = "absent"
)

type Ownership struct {
	OwnerDid repoident.OwnerDid
	Rkey     syntax.RecordKey
}

type Result struct {
	RepoDid repoident.RepoDid
	KnotURL repoident.KnotURL

	answer    Answer
	ownership Ownership
}

func Owned(repoDid repoident.RepoDid, knot repoident.KnotURL, ownership Ownership) Result {
	return Result{RepoDid: repoDid, KnotURL: knot, answer: AnswerOwner, ownership: ownership}
}

func Refused(repoDid repoident.RepoDid, knot repoident.KnotURL, answer Answer) Result {
	return Result{RepoDid: repoDid, KnotURL: knot, answer: answer}
}

func (r Result) Answer() Answer {
	return lo.Ternary(r.answer == "", AnswerUnset, r.answer)
}

func (r Result) Ownership() (Ownership, bool) {
	return r.ownership, r.answer == AnswerOwner
}

var ErrKnotAnswer = errors.New("knot returned an invalid describeRepo answer")

const repoNotFound = "RepoNotFound"

var terminalErrors = []error{
	ErrKnotAnswer,
	repoident.ErrNoKnotService,
	repoident.ErrNilIdentity,
	xrpcclient.ErrXrpcUnauthorized,
	xrpcclient.ErrXrpcForbidden,
}

func Retriable(err error) bool {
	return err != nil && !lo.ContainsBy(terminalErrors, func(t error) bool { return errors.Is(err, t) })
}

func Describe(ctx context.Context, httpClient *http.Client, knot repoident.KnotURL, repoDid repoident.RepoDid) (Result, error) {
	client := &indigoxrpc.Client{Host: knot.String(), Client: httpClient}
	out, err := tangled.RepoDescribeRepo(ctx, client, repoDid.String())
	if xrpcErr := xrpcclient.HandleXrpcErr(err); xrpcErr != nil {
		if errors.Is(xrpcErr, xrpcclient.ErrXrpcUnsupported) || errors.Is(xrpcErr, xrpcclient.ErrXrpcNotFound) {
			return Refused(repoDid, knot, lo.Ternary(xrpcclient.ErrorName(err) == repoNotFound, AnswerAbsent, AnswerNoRoute)), nil
		}
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
	return Owned(repoDid, knot, Ownership{OwnerDid: ownerDid, Rkey: rkey}), nil
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

		return Describe(ctx, httpClient, knot, repoDid)
	}
}
