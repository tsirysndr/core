package xrpc

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	securejoin "github.com/cyphar/filepath-securejoin"
	gogit "github.com/go-git/go-git/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hook"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/knotserver/repodid"
	"tangled.org/core/knotserver/sandbox"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (h *Xrpc) CreateRepo(w http.ResponseWriter, r *http.Request) {
	l := h.Logger.With("handler", "NewRepo")
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		fail(xrpcerr.MissingActorDidError)
		return
	}

	isMember, err := h.Enforcer.IsRepoCreateAllowed(actorDid.String(), rbac.ThisServer)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}
	if !isMember {
		fail(xrpcerr.AccessControlError(actorDid.String()))
		return
	}

	var data tangled.RepoCreate_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	repoName := data.Name

	if repoName == "" {
		fail(xrpcerr.GenericError(fmt.Errorf("repository name is required")))
		return
	}

	defaultBranch := h.Config.Repo.MainBranch
	if data.DefaultBranch != nil && *data.DefaultBranch != "" {
		defaultBranch = *data.DefaultBranch
	}

	if err := ValidateRepoName(repoName); err != nil {
		l.Error("creating repo", "error", err.Error())
		fail(xrpcerr.GenericError(err))
		return
	}

	var repoDid string
	var prepared *repodid.PreparedDID

	knotServiceUrl := "https://" + h.Config.Server.Hostname
	if h.Config.Server.Dev {
		knotServiceUrl = "http://" + h.Config.Server.Hostname
	}

	switch {
	case data.RepoDid != nil && strings.HasPrefix(*data.RepoDid, "did:web:"):
		if err := repodid.VerifyRepoDIDWeb(r.Context(), h.Resolver, *data.RepoDid, knotServiceUrl); err != nil {
			l.Error("verifying did:web", "error", err.Error())
			writeError(w, xrpcerr.GenericError(err), http.StatusBadRequest)
			return
		}

		exists, err := h.Db.RepoDidExists(*data.RepoDid)
		if err != nil {
			l.Error("checking did:web uniqueness", "error", err.Error())
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
		if exists {
			writeError(w, xrpcerr.GenericError(fmt.Errorf("did:web %s is already in use on this knot", *data.RepoDid)), http.StatusConflict)
			return
		}

		repoDid = *data.RepoDid

	case data.RepoDid != nil && *data.RepoDid != "":
		writeError(w, xrpcerr.GenericError(fmt.Errorf("only did:web is accepted as a user-provided repo DID; did:plc is auto-generated")), http.StatusBadRequest)
		return

	default:
		removeOrphan := func(orphanDid string) error {
			orphanPath, _ := securejoin.SecureJoin(h.Config.Repo.ScanPath, orphanDid)
			if rmErr := os.RemoveAll(orphanPath); rmErr != nil {
				l.Warn("failed to remove orphan repo directory", "path", orphanPath, "error", rmErr.Error())
			}
			if rbacErr := h.Enforcer.RemoveRepo(actorDid.String(), rbac.ThisServer, orphanDid); rbacErr != nil {
				l.Warn("failed to remove orphan rbac entry", "repoDid", orphanDid, "error", rbacErr.Error())
			}
			return h.Db.DeleteRepoKey(orphanDid)
		}

		existingDid, dbErr := h.Db.GetRepoDid(actorDid.String(), repoName)
		if dbErr != nil && !errors.Is(dbErr, sql.ErrNoRows) {
			l.Error("failed to look up repo alias", "error", dbErr.Error())
			writeError(w, xrpcerr.GenericError(dbErr), http.StatusInternalServerError)
			return
		}
		if dbErr == nil && existingDid != "" {
			didRepoPath, _ := securejoin.SecureJoin(h.Config.Repo.ScanPath, existingDid)
			if _, statErr := os.Stat(didRepoPath); statErr == nil {
				l.Info("repo already exists from previous attempt", "repoDid", existingDid)
				output := tangled.RepoCreate_Output{RepoDid: &existingDid}
				h.writeJson(w, &output)
				return
			}
			l.Warn("stale repo key found without directory, cleaning up", "repoDid", existingDid)
			if delErr := removeOrphan(existingDid); delErr != nil {
				l.Error("failed to clean up stale repo key", "repoDid", existingDid, "error", delErr.Error())
				writeError(w, xrpcerr.GenericError(fmt.Errorf("failed to clean up stale state, retry later")), http.StatusInternalServerError)
				return
			}
		} else {
			orphanDid, lookupErr := h.Db.GetRepoDidByName(actorDid.String(), repoName)
			if lookupErr != nil && !errors.Is(lookupErr, sql.ErrNoRows) {
				l.Error("failed to look up orphan repo key", "error", lookupErr.Error())
				writeError(w, xrpcerr.GenericError(lookupErr), http.StatusInternalServerError)
				return
			}
			if lookupErr == nil && orphanDid != "" {
				orphanPath, _ := securejoin.SecureJoin(h.Config.Repo.ScanPath, orphanDid)
				if _, statErr := os.Stat(orphanPath); statErr == nil {
					l.Error("orphan repo_keys row but directory present, refusing to overwrite", "repoDid", orphanDid)
					writeError(w, xrpcerr.GenericError(fmt.Errorf("repository %q is in an inconsistent state, contact a knot admin", repoName)), http.StatusConflict)
					return
				}
				l.Warn("orphan repo_keys row without alias, cleaning up", "repoDid", orphanDid)
				if delErr := removeOrphan(orphanDid); delErr != nil {
					l.Error("failed to clean up orphan repo key", "repoDid", orphanDid, "error", delErr.Error())
					writeError(w, xrpcerr.GenericError(fmt.Errorf("failed to clean up orphan state, retry later")), http.StatusInternalServerError)
					return
				}
			}
		}

		var prepErr error
		prepared, prepErr = repodid.PrepareRepoDID(h.Config.Server.PlcUrl, knotServiceUrl)
		if prepErr != nil {
			l.Error("preparing repo DID", "error", prepErr.Error())
			writeError(w, xrpcerr.GenericError(prepErr), http.StatusInternalServerError)
			return
		}
		repoDid = prepared.RepoDid

		if err := h.Db.StoreRepoKey(repoDid, prepared.SigningKeyRaw, actorDid.String(), repoName); err != nil {
			if strings.Contains(err.Error(), "UNIQUE constraint failed") {
				writeError(w, xrpcerr.GenericError(fmt.Errorf("repository %s already being created", repoName)), http.StatusConflict)
				return
			}
			l.Error("claiming repo key slot", "error", err.Error())
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
	}

	l = l.With("repoDid", repoDid)

	repoPath, _ := securejoin.SecureJoin(h.Config.Repo.ScanPath, repoDid)
	rbacPath := repoDid
	repoAddedToRBAC := false

	cleanup := func() {
		if rmErr := os.RemoveAll(repoPath); rmErr != nil {
			l.Error("failed to clean up repo directory", "path", repoPath, "error", rmErr.Error())
		}
	}

	cleanupAll := func() {
		if repoAddedToRBAC {
			if rmErr := h.Enforcer.RemoveRepo(actorDid.String(), rbac.ThisServer, rbacPath); rmErr != nil {
				l.Error("failed to clean up repo permissions", "error", rmErr.Error())
			}
		}
		cleanup()
		if delErr := h.Db.DeleteRepoKey(repoDid); delErr != nil {
			l.Error("failed to clean up repo key", "error", delErr.Error())
		}
	}

	if data.Source != nil && *data.Source != "" {
		err = git.Fork(repoPath, *data.Source, h.Config)
		if err != nil {
			l.Error("forking repo", "error", err.Error())
			cleanupAll()
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
	} else {
		err = git.InitBare(repoPath, defaultBranch)
		if err != nil {
			l.Error("initializing bare repo", "error", err.Error())
			cleanupAll()
			if errors.Is(err, gogit.ErrRepositoryAlreadyExists) {
				fail(xrpcerr.RepoExistsError("repository already exists"))
				return
			}
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
	}

	if data.RepoDid != nil && strings.HasPrefix(*data.RepoDid, "did:web:") {
		if err := h.Db.StoreRepoDidWeb(repoDid, actorDid.String(), repoName); err != nil {
			cleanupAll()
			if strings.Contains(err.Error(), "UNIQUE constraint failed") {
				writeError(w, xrpcerr.GenericError(fmt.Errorf("did:web %s is already in use", repoDid)), http.StatusConflict)
				return
			}
			l.Error("storing did:web repo entry", "error", err.Error())
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
	}

	// add perms for this user to access the repo
	err = h.Enforcer.AddRepo(actorDid.String(), rbac.ThisServer, rbacPath)
	if err != nil {
		l.Error("adding repo permissions", "error", err.Error())
		cleanupAll()
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}
	repoAddedToRBAC = true

	if err := hook.SetupRepo(
		hook.Config(
			hook.WithScanPath(h.Config.Repo.ScanPath),
			hook.WithInternalApi(h.Config.Server.InternalListenAddr),
		),
		repoPath,
	); err != nil {
		l.Error("setting up repo hooks", "error", err.Error())
		cleanupAll()
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	if prepared != nil {
		plcCtx, plcCancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer plcCancel()
		if err := prepared.Submit(plcCtx); err != nil {
			l.Error("submitting to PLC directory", "error", err.Error())
			cleanupAll()
			writeError(w, xrpcerr.GenericError(fmt.Errorf("PLC directory submission failed: %w", err)), http.StatusInternalServerError)
			return
		}
	}

	// HACK: request crawl for this repository
	// Users won't want to sync entire network from their local knotmirror.
	// Therefore, to bypass the local tap, requestCrawl directly to the knotmirror.
	go func() {
		if h.Config.Server.Dev {
			repoAt := fmt.Sprintf("at://%s/%s/%s", actorDid, tangled.RepoNSID, data.Rkey)
			rCtx, rCancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer rCancel()
			h.requestCrawl(rCtx, &tangled.SyncRequestCrawl_Input{
				Hostname:   h.Config.Server.Hostname,
				EnsureRepo: &repoAt,
			})
		}
	}()

	h.writeJson(w, &tangled.RepoCreate_Output{RepoDid: &repoDid})
}

func (h *Xrpc) requestCrawl(ctx context.Context, input *tangled.SyncRequestCrawl_Input) error {
	h.Logger.Info("requesting crawl", "mirrors", h.Config.KnotMirrors)
	for _, knotmirror := range h.Config.KnotMirrors {
		xrpcc := indigoxrpc.Client{Host: knotmirror}
		if err := tangled.SyncRequestCrawl(ctx, &xrpcc, input); err != nil {
			h.Logger.Error("error requesting crawl", "err", err)
		} else {
			h.Logger.Info("crawl requested successfully")
		}
	}
	return nil
}

var reservedRepoNames = map[string]struct{}{
	"self": {},
}

func ValidateRepoName(name string) error {
	// check for path traversal attempts
	if name == "." || name == ".." ||
		strings.Contains(name, "/") || strings.Contains(name, "\\") {
		return fmt.Errorf("Repository name contains invalid path characters")
	}

	// check for sequences that could be used for traversal when normalized
	if strings.Contains(name, "./") || strings.Contains(name, "../") ||
		strings.HasPrefix(name, ".") || strings.HasSuffix(name, ".") {
		return fmt.Errorf("Repository name contains invalid path sequence")
	}

	if len(name) == 0 {
		return fmt.Errorf("Repository name cannot be empty")
	}
	if len(name) > 100 {
		return fmt.Errorf("Repository name must be 100 characters or fewer")
	}

	// then continue with character validation
	for _, char := range name {
		if !((char >= 'a' && char <= 'z') ||
			(char >= 'A' && char <= 'Z') ||
			(char >= '0' && char <= '9') ||
			char == '-' || char == '_' || char == '.') {
			return fmt.Errorf("Repository name can only contain alphanumeric characters, periods, hyphens, and underscores")
		}
	}

	// additional check to prevent multiple sequential dots
	if strings.Contains(name, "..") {
		return fmt.Errorf("Repository name cannot contain sequential dots")
	}

	if _, reserved := reservedRepoNames[strings.ToLower(name)]; reserved {
		return fmt.Errorf("Repository name %q is reserved", name)
	}

	// if all checks pass
	return nil
}
