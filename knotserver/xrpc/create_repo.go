package xrpc

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"path/filepath"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	securejoin "github.com/cyphar/filepath-securejoin"
	gogit "github.com/go-git/go-git/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hook"
	"tangled.org/core/knotserver/git"
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

	rkey := data.Rkey

	ident, err := h.Resolver.ResolveIdent(r.Context(), actorDid.String())
	if err != nil || ident.Handle.IsInvalidHandle() {
		fail(xrpcerr.GenericError(err))
		return
	}

	xrpcc := xrpc.Client{
		Host: ident.PDSEndpoint(),
	}

	resp, err := comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoNSID, actorDid.String(), rkey)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	repo := resp.Value.Val.(*tangled.Repo)

	defaultBranch := h.Config.Repo.MainBranch
	if data.DefaultBranch != nil && *data.DefaultBranch != "" {
		defaultBranch = *data.DefaultBranch
	}

	if err := validateRepoName(repo.Name); err != nil {
		l.Error("creating repo", "error", err.Error())
		fail(xrpcerr.GenericError(err))
		return
	}

	relativeRepoPath := filepath.Join(actorDid.String(), repo.Name)
	repoPath, _ := securejoin.SecureJoin(h.Config.Repo.ScanPath, relativeRepoPath)

	if data.Source != nil && *data.Source != "" {
		err = git.Fork(repoPath, *data.Source, h.Config)
		if err != nil {
			l.Error("forking repo", "error", err.Error())
			writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
			return
		}
	} else {
		err = git.InitBare(repoPath, defaultBranch)
		if err != nil {
			l.Error("initializing bare repo", "error", err.Error())
			if errors.Is(err, gogit.ErrRepositoryAlreadyExists) {
				fail(xrpcerr.RepoExistsError("repository already exists"))
				return
			} else {
				writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
				return
			}
		}
	}

	// add perms for this user to access the repo
	err = h.Enforcer.AddRepo(actorDid.String(), rbac.ThisServer, relativeRepoPath)
	if err != nil {
		l.Error("adding repo permissions", "error", err.Error())
		writeError(w, xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	hook.SetupRepo(
		hook.Config(
			hook.WithScanPath(h.Config.Repo.ScanPath),
			hook.WithInternalApi(h.Config.Server.InternalListenAddr),
		),
		repoPath,
	)

	// HACK: request crawl for this repository
	// Users won't want to sync entire network from their local knotmirror.
	// Therefore, to bypass the local tap, requestCrawl directly to the knotmirror.
	go func() {
		if h.Config.Server.Dev {
			repoAt := fmt.Sprintf("at://%s/%s/%s", actorDid, tangled.RepoNSID, rkey)
			rCtx, rCancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer rCancel()
			h.requestCrawl(rCtx, &tangled.SyncRequestCrawl_Input{
				Hostname:   h.Config.Server.Hostname,
				EnsureRepo: &repoAt,
			})
		}
	}()

	w.WriteHeader(http.StatusOK)
}

func (h *Xrpc) requestCrawl(ctx context.Context, input *tangled.SyncRequestCrawl_Input) error {
	h.Logger.Info("requesting crawl", "mirrors", h.Config.KnotMirrors)
	for _, knotmirror := range h.Config.KnotMirrors {
		xrpcc := xrpc.Client{Host: knotmirror}
		if err := tangled.SyncRequestCrawl(ctx, &xrpcc, input); err != nil {
			h.Logger.Error("error requesting crawl", "err", err)
		} else {
			h.Logger.Info("crawl requested successfully")
		}
	}
	return nil
}

func validateRepoName(name string) error {
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

	// if all checks pass
	return nil
}
