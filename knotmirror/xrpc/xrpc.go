package xrpc

import (
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/log"
)

type Xrpc struct {
	cfg      *config.Config
	db       *sql.DB
	resolver *idresolver.Resolver
	logger   *slog.Logger
}

func New(logger *slog.Logger, cfg *config.Config, db *sql.DB, resolver *idresolver.Resolver) *Xrpc {
	return &Xrpc{
		cfg,
		db,
		resolver,
		log.SubLogger(logger, "xrpc"),
	}
}

func (x *Xrpc) Router() http.Handler {
	r := chi.NewRouter()

	r.Get("/"+tangled.GitTempGetArchiveNSID, x.GetArchive)
	r.Get("/"+tangled.GitTempGetBlobNSID, x.GetBlob)
	r.Get("/"+tangled.GitTempGetBranchNSID, x.GetBranch)
	// r.Get("/"+tangled.GitTempGetCommitNSID, x.GetCommit) // todo
	// r.Get("/"+tangled.GitTempGetDiffNSID, x.GetDiff) // todo
	// r.Get("/"+tangled.GitTempGetEntityNSID, x.GetEntity) // todo
	// r.Get("/"+tangled.GitTempGetHeadNSID, x.GetHead) // todo
	r.Get("/"+tangled.GitTempGetTagNSID, x.GetTag) // using types.Response
	r.Get("/"+tangled.GitTempGetTreeNSID, x.GetTree)
	r.Get("/"+tangled.GitTempListBranchesNSID, x.ListBranches) // wip, unknown output
	r.Get("/"+tangled.GitTempListCommitsNSID, x.ListCommits)
	r.Get("/"+tangled.GitTempListLanguagesNSID, x.ListLanguages)
	r.Get("/"+tangled.GitTempListTagsNSID, x.ListTags)

	return r
}

func writeJson(w http.ResponseWriter, status int, response any) error {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if err := json.NewEncoder(w).Encode(response); err != nil {
		return err
	}
	return nil
}

func writeErr(w http.ResponseWriter, err error) error {
	var apiErr *atclient.APIError
	if errors.As(err, &apiErr) {
		return writeJson(w, apiErr.StatusCode, atclient.ErrorBody{Name: apiErr.Name, Message: apiErr.Message})
	}
	return writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalServerError", Message: "internal server error"})
}
