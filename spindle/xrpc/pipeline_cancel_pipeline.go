package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"

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

	var input tangled.CiPipelineCancelPipeline_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	aturi := syntax.ATURI(input.Pipeline)
	pipelineId := models.PipelineId{
		Knot: strings.TrimPrefix(aturi.Authority().String(), "did:web:"),
		Rkey: aturi.RecordKey().String(),
	}

	var workflows []string
	if len(input.Workflows) > 0 {
		workflows = input.Workflows
	} else {
		// fetch workflows from db if none are specified
		p, err := x.Db.GetPipeline(r.Context(), pipelineId.Rkey)
		if err != nil {
			fail(xrpcerr.GenericError(fmt.Errorf("failed to get pipeline: %w", err)))
			return
		}
		for _, w := range p.Workflows {
			workflows = append(workflows, w.Name)
		}
	}

	if _, xerr, ok := x.resolveOwnedRepo(r.Context(), actorDid, input.Repo); !ok {
		fail(xerr)
		return
	}

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

	w.WriteHeader(http.StatusOK)
}
