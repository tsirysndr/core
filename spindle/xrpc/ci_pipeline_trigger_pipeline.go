package xrpc

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"

	"tangled.org/core/api/tangled"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) TriggerPipeline(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}
	l.Debug("trigger pipeline")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	var input tangled.CiPipelineTriggerPipeline_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	if len(input.Sha) != 40 {
		fail(xrpcerr.GenericError(fmt.Errorf("sha must be a 40-character commit hash")))
		return
	}

	repoDid, xerr, ok := x.resolveOwnedRepo(r.Context(), actorDid, input.Repo)
	if !ok {
		fail(xerr)
		return
	}

	ref := ""
	if input.Ref != nil {
		ref = *input.Ref
	}

	pipelineAt, err := x.Trigger.TriggerManual(r.Context(), repoDid, input.Sha, ref, input.Workflows)
	if errors.Is(err, ErrNoMatchingWorkflows) {
		fail(xrpcerr.GenericError(err))
		return
	}
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to trigger pipeline: %w", err)))
		return
	}

	if err := writeJson(w, http.StatusOK, tangled.CiPipelineTriggerPipeline_Output{
		Pipeline: pipelineAt.String(),
	}); err != nil {
		l.Error("failed to write response", "err", err)
	}
}

// resolveOwnedRepo resolves a repo AT-URI to DID and checks owner auth
func (x *Xrpc) resolveOwnedRepo(ctx context.Context, actorDid syntax.DID, repoAtUri string) (syntax.DID, xrpcerr.XrpcError, bool) {
	repoAt, err := syntax.ParseATURI(repoAtUri)
	if err != nil {
		return "", xrpcerr.InvalidRepoError(repoAtUri), false
	}

	ident, err := x.Resolver.ResolveIdent(ctx, repoAt.Authority().String())
	if err != nil || ident.Handle.IsInvalidHandle() {
		return "", xrpcerr.GenericError(fmt.Errorf("failed to resolve handle: %w", err)), false
	}

	xrpcc := xrpc.Client{Host: ident.PDSEndpoint()}
	resp, err := atproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
	if err != nil {
		return "", xrpcerr.GenericError(err), false
	}

	repoRec, ok := resp.Value.Val.(*tangled.Repo)
	if !ok {
		return "", xrpcerr.RepoNotFoundError, false
	}
	if repoRec.RepoDid == nil || *repoRec.RepoDid == "" {
		return "", xrpcerr.GenericError(fmt.Errorf("repo record %s has no repoDid", repoAt)), false
	}
	repoDid := *repoRec.RepoDid

	isPushAllowed, err := x.Enforcer.IsPushAllowed(actorDid.String(), rbac.ThisServer, repoDid)
	if err != nil || !isPushAllowed {
		return "", xrpcerr.AccessControlError(actorDid.String()), false
	}

	return syntax.DID(repoDid), xrpcerr.XrpcError{}, true
}
