package xrpc

import (
	"bytes"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	securejoin "github.com/cyphar/filepath-securejoin"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/jetstream"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
	"tangled.org/core/xrpc/serviceauth"
)

type Xrpc struct {
	Config      *config.Config
	Db          *db.DB
	Ingester    *jetstream.JetstreamClient
	Enforcer    *rbac.Enforcer
	Logger      *slog.Logger
	Notifier    *notifier.Notifier
	Resolver    *idresolver.Resolver
	ServiceAuth *serviceauth.ServiceAuth
}

func (x *Xrpc) Router() http.Handler {
	r := chi.NewRouter()

	r.Group(func(r chi.Router) {
		r.Use(x.ServiceAuth.VerifyServiceAuth)

		r.Post("/"+tangled.RepoSetDefaultBranchNSID, x.SetDefaultBranch)
		r.Post("/"+tangled.RepoDeleteBranchNSID, x.DeleteBranch)
		r.Post("/"+tangled.RepoCreateNSID, x.CreateRepo)
		r.Post("/"+tangled.RepoDeleteNSID, x.DeleteRepo)
		r.Post("/"+tangled.RepoForkStatusNSID, x.ForkStatus)
		r.Post("/"+tangled.RepoForkSyncNSID, x.ForkSync)
		r.Post("/"+tangled.RepoHiddenRefNSID, x.HiddenRef)
		r.Post("/"+tangled.RepoMergeNSID, x.Merge)
	})

	// merge check is an open endpoint
	//
	// TODO: should we constrain this more?
	// - we can calculate on PR submit/resubmit/gitRefUpdate etc.
	// - use ETags on clients to keep requests to a minimum
	r.Post("/"+tangled.RepoMergeCheckNSID, x.MergeCheck)

	// repo query endpoints (no auth required)
	r.Get("/"+tangled.RepoTreeNSID, x.RepoTree)
	r.Get("/"+tangled.RepoLogNSID, x.RepoLog)
	r.Get("/"+tangled.RepoBranchesNSID, x.RepoBranches)
	r.Get("/"+tangled.RepoTagsNSID, x.RepoTags)
	r.Get("/"+tangled.RepoTagNSID, x.RepoTag)
	r.Get("/"+tangled.RepoBlobNSID, x.RepoBlob)
	r.Get("/"+tangled.RepoDiffNSID, x.RepoDiff)
	r.Get("/"+tangled.RepoCompareNSID, x.RepoCompare)
	r.Get("/"+tangled.RepoGetDefaultBranchNSID, x.RepoGetDefaultBranch)
	r.Get("/"+tangled.RepoBranchNSID, x.RepoBranch)
	r.Get("/"+tangled.RepoArchiveNSID, x.RepoArchive)
	r.Get("/"+tangled.RepoLanguagesNSID, x.RepoLanguages)

	// knot query endpoints (no auth required)
	r.Get("/"+tangled.KnotListKeysNSID, x.ListKeys)
	r.Get("/"+tangled.KnotVersionNSID, x.Version)

	// service query endpoints (no auth required)
	r.Get("/"+tangled.OwnerNSID, x.Owner)

	return r
}

func (x *Xrpc) parseRepoParam(repo string) (string, error) {
	if repo == "" || !strings.HasPrefix(repo, "did:") {
		return "", xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("missing or invalid repo parameter, expected a repo DID"),
		)
	}

	if !strings.Contains(repo, "/") {
		repoPath, _, _, err := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repo)
		if err != nil {
			return "", xrpcerr.RepoNotFoundError
		}
		return repoPath, nil
	}

	parts := strings.SplitN(repo, "/", 2)
	ownerDid, repoName := parts[0], parts[1]

	repoDid, err := x.Db.GetRepoDid(ownerDid, repoName)
	if err == nil {
		repoPath, _, _, resolveErr := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repoDid)
		if resolveErr == nil {
			return repoPath, nil
		}
	}

	repoPath, joinErr := securejoin.SecureJoin(x.Config.Repo.ScanPath, filepath.Join(ownerDid, repoName))
	if joinErr != nil {
		return "", xrpcerr.RepoNotFoundError
	}
	if _, statErr := os.Stat(repoPath); statErr != nil {
		return "", xrpcerr.RepoNotFoundError
	}
	return repoPath, nil
}

func writeError(w http.ResponseWriter, e xrpcerr.XrpcError, status int) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(e)
}

type limitWriter struct {
	buf     bytes.Buffer
	limit   int
	written int
}

var errResponseTooLarge = errors.New("response too large")

func (lw *limitWriter) Write(p []byte) (int, error) {
	if lw.written+len(p) > lw.limit {
		return 0, errResponseTooLarge
	}
	n, err := lw.buf.Write(p)
	lw.written += n
	return n, err
}

func (x *Xrpc) writeJson(w http.ResponseWriter, response any) {
	lw := &limitWriter{limit: x.Config.Server.MaxResponseKB * 1024}
	if err := json.NewEncoder(lw).Encode(response); err != nil {
		if errors.Is(err, errResponseTooLarge) {
			writeError(w, xrpcerr.RequestTooLargeError, http.StatusRequestEntityTooLarge)
		} else {
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		}
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Write(lw.buf.Bytes())
}
