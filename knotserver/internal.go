package knotserver

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	securejoin "github.com/cyphar/filepath-securejoin"
	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/hook"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/log"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/workflow"
)

type InternalHandle struct {
	db  *db.DB
	c   *config.Config
	e   *rbac.Enforcer
	l   *slog.Logger
	n   *notifier.Notifier
	res *idresolver.Resolver
}

func (h *InternalHandle) PushAllowed(w http.ResponseWriter, r *http.Request) {
	user := r.URL.Query().Get("user")
	repo := r.URL.Query().Get("repo")

	if user == "" || repo == "" {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	ok, err := h.e.IsPushAllowed(user, rbac.ThisServer, repo)
	if err != nil || !ok {
		w.WriteHeader(http.StatusForbidden)
		return
	}

	w.WriteHeader(http.StatusNoContent)
}

func (h *InternalHandle) InternalKeys(w http.ResponseWriter, r *http.Request) {
	keys, err := h.db.GetAllPublicKeys()
	if err != nil {
		writeError(w, err.Error(), http.StatusInternalServerError)
		return
	}

	data := make([]map[string]interface{}, 0)
	for _, key := range keys {
		j := key.JSON()
		data = append(data, j)
	}
	writeJSON(w, data)
}

// response in text/plain format
// the body will be qualified repository path on success/push-denied
// or an error message when process failed
func (h *InternalHandle) Guard(w http.ResponseWriter, r *http.Request) {
	l := h.l.With("handler", "Guard")

	var (
		incomingUser = r.URL.Query().Get("user")
		repo         = r.URL.Query().Get("repo")
		gitCommand   = r.URL.Query().Get("gitCmd")
	)

	if incomingUser == "" || repo == "" || gitCommand == "" {
		w.WriteHeader(http.StatusBadRequest)
		l.Error("invalid request", "incomingUser", incomingUser, "repo", repo, "gitCommand", gitCommand)
		fmt.Fprintln(w, "invalid internal request")
		return
	}

	components := strings.Split(strings.TrimPrefix(strings.Trim(repo, "'"), "/"), "/")
	l.Info("command components", "components", components)

	var rbacResource string
	var diskRelative string

	switch {
	case len(components) == 1 && strings.HasPrefix(components[0], "did:"):
		repoDid := components[0]
		repoPath, _, _, lookupErr := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
		if lookupErr != nil {
			w.WriteHeader(http.StatusNotFound)
			l.Error("repo DID not found", "repoDid", repoDid, "err", lookupErr)
			fmt.Fprintln(w, "repo not found")
			return
		}
		rbacResource = repoDid
		rel, relErr := filepath.Rel(h.c.Repo.ScanPath, repoPath)
		if relErr != nil {
			w.WriteHeader(http.StatusInternalServerError)
			l.Error("failed to compute relative path", "repoPath", repoPath, "err", relErr)
			fmt.Fprintln(w, "internal error")
			return
		}
		diskRelative = rel

	case len(components) == 2:
		repoOwner := components[0]
		resolver := idresolver.DefaultResolver(h.c.Server.PlcUrl)
		repoOwnerIdent, resolveErr := resolver.ResolveIdent(r.Context(), repoOwner)
		if resolveErr != nil || repoOwnerIdent.Handle.IsInvalidHandle() {
			l.Error("Error resolving handle", "handle", repoOwner, "err", resolveErr)
			w.WriteHeader(http.StatusInternalServerError)
			fmt.Fprintf(w, "error resolving handle: invalid handle\n")
			return
		}
		ownerDid := repoOwnerIdent.DID.String()
		repoName := components[1]
		repoDid, didErr := h.db.GetRepoDid(ownerDid, repoName)
		var repoPath string
		if didErr == nil {
			var lookupErr error
			repoPath, _, _, lookupErr = h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
			if lookupErr != nil {
				w.WriteHeader(http.StatusNotFound)
				l.Error("repo not found on disk", "repoDid", repoDid, "err", lookupErr)
				fmt.Fprintln(w, "repo not found")
				return
			}
			rbacResource = repoDid
		} else {
			legacyPath, joinErr := securejoin.SecureJoin(h.c.Repo.ScanPath, filepath.Join(ownerDid, repoName))
			if joinErr != nil {
				w.WriteHeader(http.StatusNotFound)
				fmt.Fprintln(w, "repo not found")
				return
			}
			if _, statErr := os.Stat(legacyPath); statErr != nil {
				w.WriteHeader(http.StatusNotFound)
				l.Error("repo not found on disk (legacy)", "owner", ownerDid, "name", repoName)
				fmt.Fprintln(w, "repo not found")
				return
			}
			repoPath = legacyPath
			rbacResource = ownerDid + "/" + repoName
		}
		rel, relErr := filepath.Rel(h.c.Repo.ScanPath, repoPath)
		if relErr != nil {
			w.WriteHeader(http.StatusInternalServerError)
			l.Error("failed to compute relative path", "repoPath", repoPath, "err", relErr)
			fmt.Fprintln(w, "internal error")
			return
		}
		diskRelative = rel

	default:
		w.WriteHeader(http.StatusBadRequest)
		l.Error("invalid repo format", "components", components)
		fmt.Fprintln(w, "invalid repo format, needs <user>/<repo>, /<user>/<repo>, or <repo-did>")
		return
	}

	if gitCommand == "git-receive-pack" {
		ok, err := h.e.IsPushAllowed(incomingUser, rbac.ThisServer, rbacResource)
		if err != nil || !ok {
			w.WriteHeader(http.StatusForbidden)
			fmt.Fprint(w, repo)
			return
		}
	}

	w.WriteHeader(http.StatusOK)
	fmt.Fprint(w, diskRelative)
}

type PushOptions struct {
	skipCi    bool
	verboseCi bool
}

func (h *InternalHandle) PostReceiveHook(w http.ResponseWriter, r *http.Request) {
	l := h.l.With("handler", "PostReceiveHook")

	gitAbsoluteDir := r.Header.Get("X-Git-Dir")
	gitRelativeDir, err := filepath.Rel(h.c.Repo.ScanPath, gitAbsoluteDir)
	if err != nil {
		l.Error("failed to calculate relative git dir", "scanPath", h.c.Repo.ScanPath, "gitAbsoluteDir", gitAbsoluteDir)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	var repoDid string
	var ownerDid, repoName string

	if strings.HasPrefix(gitRelativeDir, "did:") {
		repoDid = gitRelativeDir
		var err error
		ownerDid, repoName, err = h.db.GetRepoKeyOwner(repoDid)
		if err != nil {
			l.Error("failed to resolve repo DID from git dir", "repoDid", repoDid, "err", err)
			w.WriteHeader(http.StatusBadRequest)
			return
		}
	} else {
		components := strings.SplitN(gitRelativeDir, "/", 2)
		if len(components) != 2 {
			l.Error("invalid git dir, expected repo DID or owner/repo", "gitRelativeDir", gitRelativeDir)
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		ownerDid = components[0]
		repoName = components[1]
		var didErr error
		repoDid, didErr = h.db.GetRepoDid(ownerDid, repoName)
		if didErr != nil {
			l.Error("failed to resolve repo DID from legacy path", "gitRelativeDir", gitRelativeDir, "err", didErr)
			w.WriteHeader(http.StatusBadRequest)
			return
		}
	}

	gitUserDid := r.Header.Get("X-Git-User-Did")

	lines, err := git.ParsePostReceive(r.Body)
	if err != nil {
		l.Error("failed to parse post-receive payload", "err", err)
		// non-fatal
	}

	// extract any push options
	pushOptionsRaw := r.Header.Values("X-Git-Push-Option")
	pushOptions := PushOptions{}
	for _, option := range pushOptionsRaw {
		if option == "skip-ci" || option == "ci-skip" {
			pushOptions.skipCi = true
		}
		if option == "verbose-ci" || option == "ci-verbose" {
			pushOptions.verboseCi = true
		}
	}

	resp := hook.HookResponse{
		Messages: make([]string, 0),
	}

	for _, line := range lines {
		err := h.insertRefUpdate(line, gitUserDid, ownerDid, repoName, repoDid)
		if err != nil {
			l.Error("failed to insert op", "err", err, "line", line, "did", gitUserDid, "repo", gitRelativeDir)
		}

		err = h.emitCompareLink(&resp.Messages, line, ownerDid, repoName, repoDid)
		if err != nil {
			l.Error("failed to reply with compare link", "err", err, "line", line, "did", gitUserDid, "repo", gitRelativeDir)
		}

		err = h.triggerPipeline(&resp.Messages, line, gitUserDid, ownerDid, repoName, repoDid, pushOptions)
		if err != nil {
			l.Error("failed to trigger pipeline", "err", err, "line", line, "did", gitUserDid, "repo", gitRelativeDir)
		}
	}

	writeJSON(w, resp)
}

func (h *InternalHandle) insertRefUpdate(line git.PostReceiveLine, gitUserDid, ownerDid, repoName, repoDid string) error {
	repoPath, _, _, resolveErr := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
	if resolveErr != nil {
		return fmt.Errorf("failed to resolve repo on disk: %w", resolveErr)
	}

	gr, err := git.Open(repoPath, line.Ref)
	if err != nil {
		return fmt.Errorf("failed to open git repo at ref %s: %w", line.Ref, err)
	}

	meta, err := gr.RefUpdateMeta(line)
	if err != nil {
		return fmt.Errorf("failed to get ref update metadata: %w", err)
	}

	metaRecord := meta.AsRecord()

	refUpdate := tangled.GitRefUpdate{
		OldSha:       line.OldSha.String(),
		NewSha:       line.NewSha.String(),
		Ref:          line.Ref,
		CommitterDid: gitUserDid,
		OwnerDid:     &ownerDid,
		RepoName:     repoName,
		RepoDid:      &repoDid,
		Meta:         &metaRecord,
	}

	eventJson, err := json.Marshal(refUpdate)
	if err != nil {
		return err
	}

	event := db.Event{
		Rkey:      TID(),
		Nsid:      tangled.GitRefUpdateNSID,
		EventJson: string(eventJson),
	}

	return h.db.InsertEvent(event, h.n)
}

func (h *InternalHandle) triggerPipeline(
	clientMsgs *[]string,
	line git.PostReceiveLine,
	gitUserDid string,
	ownerDid string,
	repoName string,
	repoDid string,
	pushOptions PushOptions,
) error {
	if pushOptions.skipCi {
		return nil
	}

	repoPath, _, _, resolveErr := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
	if resolveErr != nil {
		return fmt.Errorf("failed to resolve repo on disk: %w", resolveErr)
	}

	gr, err := git.Open(repoPath, line.Ref)
	if err != nil {
		return err
	}

	workflowDir, err := gr.FileTree(context.Background(), workflow.WorkflowDir)
	if err != nil {
		return err
	}

	var pipeline workflow.RawPipeline
	for _, e := range workflowDir {
		if !e.IsFile() {
			continue
		}

		fpath := filepath.Join(workflow.WorkflowDir, e.Name)
		contents, err := gr.RawContent(fpath)
		if err != nil {
			continue
		}

		pipeline = append(pipeline, workflow.RawWorkflow{
			Name:     e.Name,
			Contents: contents,
		})
	}

	trigger := tangled.Pipeline_PushTriggerData{
		Ref:    line.Ref,
		OldSha: line.OldSha.String(),
		NewSha: line.NewSha.String(),
	}

	triggerRepo := &tangled.Pipeline_TriggerRepo{
		Did:     ownerDid,
		Knot:    h.c.Server.Hostname,
		Repo:    &repoName,
		RepoDid: &repoDid,
	}

	compiler := workflow.Compiler{
		Trigger: tangled.Pipeline_TriggerMetadata{
			Kind: string(workflow.TriggerKindPush),
			Push: &trigger,
			Repo: triggerRepo,
		},
	}

	cp := compiler.Compile(compiler.Parse(pipeline))
	eventJson, err := json.Marshal(cp)
	if err != nil {
		return err
	}

	for _, e := range compiler.Diagnostics.Errors {
		*clientMsgs = append(*clientMsgs, e.String())
	}

	if pushOptions.verboseCi {
		if compiler.Diagnostics.IsEmpty() {
			*clientMsgs = append(*clientMsgs, "success: pipeline compiled with no diagnostics")
		}

		for _, w := range compiler.Diagnostics.Warnings {
			*clientMsgs = append(*clientMsgs, w.String())
		}
	}

	// do not run empty pipelines
	if cp.Workflows == nil {
		return nil
	}

	event := db.Event{
		Rkey:      TID(),
		Nsid:      tangled.PipelineNSID,
		EventJson: string(eventJson),
	}

	return h.db.InsertEvent(event, h.n)
}

func (h *InternalHandle) emitCompareLink(
	clientMsgs *[]string,
	line git.PostReceiveLine,
	ownerDid string,
	repoName string,
	repoDid string,
) error {
	// this is a second push to a branch, don't reply with the link again
	if !line.OldSha.IsZero() {
		return nil
	}

	// the ref was not updated to a new hash, don't reply with the link
	//
	// NOTE: do we need this?
	if line.NewSha.String() == line.OldSha.String() {
		return nil
	}

	pushedRef := plumbing.ReferenceName(line.Ref)

	userIdent, err := h.res.ResolveIdent(context.Background(), ownerDid)
	user := ownerDid
	if err == nil {
		user = userIdent.Handle.String()
	}

	repoPath, _, _, resolveErr := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
	if resolveErr != nil {
		return fmt.Errorf("failed to resolve repo on disk: %w", resolveErr)
	}

	gr, err := git.PlainOpen(repoPath)
	if err != nil {
		return err
	}

	defaultBranch, err := gr.FindMainBranch()
	if err != nil {
		return err
	}

	// pushing to default branch
	if pushedRef == plumbing.NewBranchReferenceName(defaultBranch) {
		return nil
	}

	// pushing a tag, don't prompt the user the open a PR
	if pushedRef.IsTag() {
		return nil
	}

	ZWS := "\u200B"
	*clientMsgs = append(*clientMsgs, ZWS)
	*clientMsgs = append(*clientMsgs, fmt.Sprintf("Create a PR pointing to %s", defaultBranch))
	*clientMsgs = append(*clientMsgs, fmt.Sprintf("\t%s/%s/%s/compare/%s...%s", h.c.AppViewEndpoint, user, repoName, defaultBranch, strings.TrimPrefix(line.Ref, "refs/heads/")))
	*clientMsgs = append(*clientMsgs, ZWS)
	return nil
}

func Internal(ctx context.Context, c *config.Config, db *db.DB, e *rbac.Enforcer, n *notifier.Notifier) http.Handler {
	r := chi.NewRouter()
	l := log.FromContext(ctx)
	l = log.SubLogger(l, "internal")
	res := idresolver.DefaultResolver(c.Server.PlcUrl)

	h := InternalHandle{
		db,
		c,
		e,
		l,
		n,
		res,
	}

	r.Get("/push-allowed", h.PushAllowed)
	r.Get("/keys", h.InternalKeys)
	r.Get("/guard", h.Guard)
	r.Post("/hooks/post-receive", h.PostReceiveHook)
	r.Mount("/debug", middleware.Profiler())

	return r
}
