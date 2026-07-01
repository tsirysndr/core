package xrpc

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"

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

	var input tangled.CiTriggerPipeline_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	repoDid, xerr, ok := x.resolveOwnedRepo(r.Context(), actorDid, input.Repo)
	if !ok {
		fail(xerr)
		return
	}

	var sha string
	ref := ""
	var sourceRepo syntax.DID
	var pull PullContext
	var inputs []*tangled.Pipeline_Pair

	switch {
	case input.Trigger == nil:
		fail(xrpcerr.GenericError(fmt.Errorf("trigger is required")))
		return

	case input.Trigger.CiTrigger_Manual != nil:
		manual := input.Trigger.CiTrigger_Manual
		sha = manual.Sha
		if manual.Ref != nil {
			ref = *manual.Ref
		}
		parsed, err := parseOptionalDID("sourceRepo", manual.SourceRepo)
		if err != nil {
			fail(xrpcerr.GenericError(err))
			return
		}
		sourceRepo = parsed
		inputs = ciTriggerPairsToPipelinePairs(manual.Inputs)

	case input.Trigger.CiTrigger_PullRequest != nil:
		pr := input.Trigger.CiTrigger_PullRequest
		sha = pr.SourceSha
		parsed, err := parseOptionalDID("sourceRepo", pr.SourceRepo)
		if err != nil {
			fail(xrpcerr.GenericError(err))
			return
		}
		sourceRepo = parsed

		if pr.TargetBranch == "" {
			fail(xrpcerr.GenericError(fmt.Errorf("pull request trigger targetBranch is required")))
			return
		}

		var pullAt syntax.ATURI
		if pr.Pull != nil {
			var err error
			pullAt, err = syntax.ParseATURI(*pr.Pull)
			if err != nil {
				fail(xrpcerr.InvalidRepoError(*pr.Pull))
				return
			}
		}
		sourceBranch := ""
		if pr.SourceBranch != nil {
			sourceBranch = *pr.SourceBranch
		}
		pull = PullContext{
			IsPullRequest: true,
			Pull:          pullAt,
			SourceBranch:  sourceBranch,
			TargetBranch:  pr.TargetBranch,
		}

	default:
		fail(xrpcerr.GenericError(fmt.Errorf("unsupported trigger variant")))
		return
	}

	if len(sha) != 40 {
		fail(xrpcerr.GenericError(fmt.Errorf("sha must be a 40-character commit hash")))
		return
	}

	pipelineAt, err := x.Trigger.TriggerManual(r.Context(), repoDid, sha, ref, input.Workflows, sourceRepo, pull, inputs)
	if errors.Is(err, ErrNoMatchingWorkflows) {
		fail(xrpcerr.GenericError(err))
		return
	}
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to trigger pipeline: %w", err)))
		return
	}

	if err := writeJson(w, http.StatusOK, tangled.CiTriggerPipeline_Output{
		Pipeline: pipelineAt.String(),
	}); err != nil {
		l.Error("failed to write response", "err", err)
	}
}

func parseOptionalDID(field string, value *string) (syntax.DID, error) {
	if value == nil || *value == "" {
		return "", nil
	}
	did, err := syntax.ParseDID(*value)
	if err != nil {
		return "", fmt.Errorf("invalid %s DID %q: %w", field, *value, err)
	}
	return did, nil
}

func ciTriggerPairsToPipelinePairs(inputs []*tangled.CiTrigger_Pair) []*tangled.Pipeline_Pair {
	if len(inputs) == 0 {
		return nil
	}
	pairs := make([]*tangled.Pipeline_Pair, 0, len(inputs))
	for _, input := range inputs {
		if input == nil {
			continue
		}
		pairs = append(pairs, &tangled.Pipeline_Pair{
			Key:   input.Key,
			Value: input.Value,
		})
	}
	return pairs
}

// resolveOwnedRepo resolves a repository DID and checks push auth.
func (x *Xrpc) resolveOwnedRepo(ctx context.Context, actorDid syntax.DID, repoDidStr string) (syntax.DID, xrpcerr.XrpcError, bool) {
	repoDid, xerr, ok := x.resolveKnownRepoDid(repoDidStr)
	if !ok {
		return "", xerr, false
	}

	isPushAllowed, err := x.Enforcer.IsPushAllowed(actorDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || !isPushAllowed {
		return "", xrpcerr.AccessControlError(actorDid.String()), false
	}

	return repoDid, xrpcerr.XrpcError{}, true
}

func (x *Xrpc) resolveKnownRepoDid(repoDidStr string) (syntax.DID, xrpcerr.XrpcError, bool) {
	repoDid, err := syntax.ParseDID(repoDidStr)
	if err != nil {
		return "", xrpcerr.GenericError(fmt.Errorf("invalid repo DID %q: %w", repoDidStr, err)), false
	}

	if _, err := x.Db.GetRepoByDid(repoDid); err != nil {
		return "", xrpcerr.RepoNotFoundError, false
	}

	return repoDid, xrpcerr.XrpcError{}, true
}
