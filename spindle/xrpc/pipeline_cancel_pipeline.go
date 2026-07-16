package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) CancelPipeline(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}
	l.Debug("cancel pipeline")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	var input tangled.CiCancelPipeline_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	pipelineTid, err := syntax.ParseTID(input.Pipeline)
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("invalid pipeline TID %q: %w", input.Pipeline, err)))
		return
	}

	repoDid, xerr, ok := x.resolveOwnedRepo(r.Context(), actorDid, input.Repo)
	if !ok {
		fail(xerr)
		return
	}
	repo, err := x.Db.GetRepoByDid(repoDid)
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to get repo: %w", err)))
		return
	}

	// the actor is only authorized against input.Repo, so make sure the
	// pipeline actually belongs to it before cancelling anything
	p, err := x.Db.GetPipeline(r.Context(), pipelineTid.String())
	if err != nil {
		fail(xrpcerr.GenericError(fmt.Errorf("failed to get pipeline: %w", err)))
		return
	}
	if p.Repo == nil || *p.Repo != repoDid.String() {
		fail(xrpcerr.AccessControlError(actorDid.String()))
		return
	}

	pipelineId := models.PipelineId{
		Knot: repo.Knot,
		Rkey: pipelineTid.String(),
	}
	l = l.With("input.pipeline", pipelineTid, "input.workflows", input.Workflows)

	workflows := input.Workflows
	if len(workflows) == 0 {
		// cancel every workflow when none are specified
		for _, w := range p.Workflows {
			workflows = append(workflows, w.Name)
		}
	}

	canceled := false
	defer func() {
		l.Debug("canceled pipeline", "canceled", canceled)
	}()

	for _, wName := range workflows {
		wid := models.WorkflowId{
			PipelineId: pipelineId,
			Name:       wName,
		}
		l.Debug("cancel pipeline", "wid", wid)

		for _, engine := range x.Engines {
			l.Debug("destroying workflow", "wid", wid)
			err := engine.DestroyWorkflow(r.Context(), wid)
			if err != nil {
				fail(xrpcerr.GenericError(fmt.Errorf("failed to destroy workflow: %w", err)))
				return
			}
			err = x.Db.StatusCancelled(wid, "User canceled the workflow", -1, x.Notifier)
			if err != nil {
				fail(xrpcerr.GenericError(fmt.Errorf("failed to emit status failed: %w", err)))
				return
			}
		}
	}

	canceled = true

	w.WriteHeader(http.StatusOK)
}
