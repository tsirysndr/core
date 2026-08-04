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
	"tangled.org/core/gitutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/sandbox"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/repoident"
	xrpcerr "tangled.org/core/xrpc/errors"
	"tangled.org/core/xrpc/serviceauth"
)

const ActorDid = serviceauth.ActorDid

type DidIngester interface {
	AddDid(did string)
	RemoveDid(did string)
}

type Xrpc struct {
	Config      *config.Config
	Db          *db.DB
	Ingester    DidIngester
	Enforcer    *rbac.Enforcer
	Logger      *slog.Logger
	Notifier    *notifier.Notifier
	Resolver    *idresolver.Resolver
	ServiceAuth *serviceauth.ServiceAuth
	Sandbox     sandbox.Backend
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
		r.Post("/"+tangled.KnotAddMemberNSID, x.AddMember)
		r.Post("/"+tangled.KnotRemoveMemberNSID, x.RemoveMember)
		r.Post("/"+tangled.RepoAddCollaboratorNSID, x.AddCollaborator)
		r.Post("/"+tangled.RepoRemoveCollaboratorNSID, x.RemoveCollaborator)
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
	r.Get("/"+tangled.RepoDescribeRepoNSID, x.RepoDescribeRepo)
	r.Get("/"+tangled.RepoBranchNSID, x.RepoBranch)
	r.Get("/"+tangled.RepoArchiveNSID, x.RepoArchive)
	r.Get("/"+tangled.RepoLanguagesNSID, x.RepoLanguages)
	r.Get("/"+tangled.RepoListCollaboratorsNSID, x.ListCollaborators)

	// knot query endpoints (no auth required)
	r.Get("/"+tangled.KnotListKeysNSID, x.ListKeys)
	r.Get("/"+tangled.KnotListMembersNSID, x.ListMembers)
	r.Get("/"+tangled.KnotVersionNSID, x.Version)

	// service query endpoints (no auth required)
	r.Get("/"+tangled.OwnerNSID, x.Owner)

	return r
}

type resolvedRepo struct {
	path string
	name gitutil.RepoName
}

func (x *Xrpc) parseRepoParam(repo string) (string, error) {
	resolved, err := x.resolveRepo(repo)
	return resolved.path, err
}

func (x *Xrpc) resolveRepo(repo string) (resolvedRepo, error) {
	if repo == "" || !strings.HasPrefix(repo, "did:") {
		return resolvedRepo{}, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("missing or invalid repo parameter, expected a repo DID"),
		)
	}

	if !strings.Contains(repo, "/") {
		repoPath, _, repoName, err := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repo)
		if err != nil {
			return resolvedRepo{}, xrpcerr.RepoNotFoundError
		}
		return resolvedRepo{path: repoPath, name: gitutil.RepoName(repoName)}, nil
	}

	parts := strings.SplitN(repo, "/", 2)
	ownerDid, repoName := parts[0], parts[1]

	repoDid, err := x.Db.GetRepoDid(ownerDid, repoName)
	if err == nil {
		repoPath, _, _, resolveErr := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repoDid)
		if resolveErr == nil {
			return resolvedRepo{path: repoPath, name: gitutil.RepoName(repoName)}, nil
		}
	}

	repoPath, joinErr := securejoin.SecureJoin(x.Config.Repo.ScanPath, filepath.Join(ownerDid, repoName))
	if joinErr != nil {
		return resolvedRepo{}, xrpcerr.RepoNotFoundError
	}
	if _, statErr := os.Stat(repoPath); statErr != nil {
		return resolvedRepo{}, xrpcerr.RepoNotFoundError
	}
	return resolvedRepo{path: repoPath, name: gitutil.RepoName(repoName)}, nil
}

func (x *Xrpc) resolveRepoDID(repo *string, ownerDid, name string) (repoident.RepoDid, string, error) {
	raw, err := x.selectRepoDID(repo, ownerDid, name)
	if err != nil {
		return "", "", err
	}

	repoDid, err := repoident.NewRepoDid(raw)
	if err != nil {
		return "", "", err
	}

	repoPath, _, _, err := x.Db.ResolveRepoDIDOnDisk(x.Config.Repo.ScanPath, repoDid.String())
	if err != nil {
		return "", "", err
	}
	return repoDid, repoPath, nil
}

func (x *Xrpc) selectRepoDID(repo *string, ownerDid, name string) (string, error) {
	if repo != nil && *repo != "" {
		return *repo, nil
	}
	return x.Db.GetRepoDid(ownerDid, name)
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
