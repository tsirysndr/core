package xrpc

import (
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"

	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
	xrpcerr "tangled.org/core/xrpc/errors"
	"tangled.org/core/xrpc/serviceauth"
)

const ActorDid = serviceauth.ActorDid

var ErrNoMatchingWorkflows = errors.New("no workflows to run")

func requireSha(sha string) error {
	if len(sha) != 40 {
		return fmt.Errorf("sha must be a 40-character commit hash")
	}
	return nil
}

// this is to break an import cycle. spindle imports this package for Xrpc,
// so this package can't import *spindle.Spindle back.
type PipelineTrigger interface {
	TriggerManual(ctx context.Context, repoDid syntax.DID, sha, ref string, workflows []string, sourceRepo syntax.DID, pull PullContext, inputs []*tangled.Pipeline_Pair) (syntax.ATURI, error)
	DescribeWorkflowDefinition(ctx context.Context, repoDid syntax.DID, sha string, sourceRepo syntax.DID) (*tangled.CiDescribeWorkflowDefinition_Output, error)
}

type PullContext struct {
	IsPullRequest bool
	Pull          syntax.ATURI
	SourceBranch  string
	TargetBranch  string
}

type Xrpc struct {
	Logger      *slog.Logger
	Db          *db.DB
	Enforcer    *rbac.Enforcer
	Engines     map[string]models.Engine
	Config      *config.Config
	Resolver    *idresolver.Resolver
	Vault       secrets.Manager
	Notifier    *notifier.Notifier
	ServiceAuth *serviceauth.ServiceAuth
	Trigger     PipelineTrigger
}

func (x *Xrpc) Router() http.Handler {
	r := chi.NewRouter()

	r.Group(func(r chi.Router) {
		r.Use(x.ServiceAuth.VerifyServiceAuth)

		r.Post("/"+tangled.RepoAddSecretNSID, x.AddSecret)
		r.Post("/"+tangled.RepoRemoveSecretNSID, x.RemoveSecret)
		r.Get("/"+tangled.RepoListSecretsNSID, x.ListSecrets)
		r.Post("/"+tangled.CiCancelPipelineNSID, x.CancelPipeline)
		r.Post("/"+tangled.CiTriggerPipelineNSID, x.TriggerPipeline)
	})

	// service query endpoints (no auth required)
	r.Get("/"+tangled.OwnerNSID, x.Owner)
	r.Get("/"+tangled.CiDescribeWorkflowDefinitionNSID, x.DescribeWorkflowDefinition)
	r.Get("/"+tangled.CiSubscribePipelineLogsNSID, x.HandleCiSubscribePipelineLogs)
	r.Get("/"+tangled.CiQueryPipelinesNSID, x.HandleCiQueryPipelines)
	r.Get("/"+tangled.CiGetPipelineNSID, x.HandleCiGetPipeline)

	return r
}

// this is slightly different from http_util::write_error to follow the spec:
//
// the json object returned must include an "error" and a "message"
func writeError(w http.ResponseWriter, e xrpcerr.XrpcError, status int) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(e)
}

func writeJson(w http.ResponseWriter, status int, response any) error {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	return json.NewEncoder(w).Encode(response)
}
