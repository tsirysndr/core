package xrpc

import (
	_ "embed"
	"encoding/json"
	"log/slog"
	"net/http"

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
}

func (x *Xrpc) Router() http.Handler {
	r := chi.NewRouter()

	r.Group(func(r chi.Router) {
		r.Use(x.ServiceAuth.VerifyServiceAuth)

		r.Post("/"+tangled.RepoAddSecretNSID, x.AddSecret)
		r.Post("/"+tangled.RepoRemoveSecretNSID, x.RemoveSecret)
		r.Get("/"+tangled.RepoListSecretsNSID, x.ListSecrets)
		r.Post("/"+tangled.PipelineCancelPipelineNSID, x.CancelPipeline)
	})

	// service query endpoints (no auth required)
	r.Get("/"+tangled.OwnerNSID, x.Owner)
	r.Get("/"+tangled.CiPipelineSubscribeLogsNSID, x.HandleCiPipelineSubscribeLogs)
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
	if err := json.NewEncoder(w).Encode(response); err != nil {
		return err
	}
	return nil
}
