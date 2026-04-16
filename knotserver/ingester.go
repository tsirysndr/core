package knotserver

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"path/filepath"
	"strings"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/log"
	"tangled.org/core/rbac"
	"tangled.org/core/workflow"
)

func (h *Knot) processPublicKey(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx)
	raw := json.RawMessage(event.Commit.Record)
	did := event.Did

	var record tangled.PublicKey
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal record: %w", err)
	}

	pk := db.PublicKey{
		Did:       did,
		PublicKey: record,
	}
	if err := h.db.AddPublicKey(pk); err != nil {
		l.Error("failed to add public key", "error", err)
		return fmt.Errorf("failed to add public key: %w", err)
	}
	l.Info("added public key from firehose", "did", did)
	return nil
}

func (h *Knot) processKnotMember(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx)
	raw := json.RawMessage(event.Commit.Record)
	did := event.Did

	var record tangled.KnotMember
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal record: %w", err)
	}

	if record.Domain != h.c.Server.Hostname {
		l.Error("domain mismatch", "domain", record.Domain, "expected", h.c.Server.Hostname)
		return fmt.Errorf("domain mismatch: %s != %s", record.Domain, h.c.Server.Hostname)
	}

	ok, err := h.e.E.Enforce(did, rbac.ThisServer, rbac.ThisServer, "server:invite")
	if err != nil || !ok {
		l.Error("failed to add member", "did", did)
		return fmt.Errorf("failed to enforce permissions: %w", err)
	}

	if err := h.e.AddKnotMember(rbac.ThisServer, record.Subject); err != nil {
		l.Error("failed to add member", "error", err)
		return fmt.Errorf("failed to add member: %w", err)
	}
	l.Info("added member from firehose", "member", record.Subject)

	if err := h.db.AddDid(record.Subject); err != nil {
		l.Error("failed to add did", "error", err)
		return fmt.Errorf("failed to add did: %w", err)
	}
	h.jc.AddDid(record.Subject)

	if err := h.fetchAndAddKeys(ctx, record.Subject); err != nil {
		return fmt.Errorf("failed to fetch and add keys: %w", err)
	}

	return nil
}

// returns a repo path on disk if present, and error if not
type targetRepo struct {
	RepoPath string
	OwnerDid string
	RepoName string
	RepoDid  string
}

func (h *Knot) validatePullRecord(ctx context.Context, record *tangled.RepoPull) (*targetRepo, error) {
	if record.Target == nil {
		return nil, fmt.Errorf("ignoring pull record: target repo is nil")
	}

	if record.Source == nil {
		return nil, fmt.Errorf("ignoring pull record: not a branch-based pull request")
	}

	if record.Source.Repo != nil || record.Source.RepoDid != nil {
		return nil, fmt.Errorf("ignoring pull record: fork based pull")
	}

	var repoPath, ownerDid, repoName, repoDid string
	switch {
	case record.Target.RepoDid != nil && *record.Target.RepoDid != "":
		repoDid = *record.Target.RepoDid
		var lookupErr error
		repoPath, ownerDid, repoName, lookupErr = h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
		if lookupErr != nil {
			return nil, fmt.Errorf("unknown target repo DID %s: %w", repoDid, lookupErr)
		}

	case record.Target.Repo != nil:
		// TODO: get rid of this PDS fetch once all repos have DIDs
		repoAt, parseErr := syntax.ParseATURI(*record.Target.Repo)
		if parseErr != nil {
			return nil, fmt.Errorf("failed to parse ATURI: %w", parseErr)
		}

		ident, resolveErr := h.resolver.ResolveIdent(ctx, repoAt.Authority().String())
		if resolveErr != nil || ident.Handle.IsInvalidHandle() {
			return nil, fmt.Errorf("failed to resolve handle: %w", resolveErr)
		}

		xrpcc := xrpc.Client{
			Host: ident.PDSEndpoint(),
		}

		resp, getErr := comatproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
		if getErr != nil {
			return nil, fmt.Errorf("failed to resolve repo: %w", getErr)
		}

		repo := resp.Value.Val.(*tangled.Repo)

		if repo.Knot != h.c.Server.Hostname {
			return nil, fmt.Errorf("rejected pull record: not this knot, %s != %s", repo.Knot, h.c.Server.Hostname)
		}

		ownerDid = ident.DID.String()
		repoName = repo.Name

		repoDid, didErr := h.db.GetRepoDid(ownerDid, repoName)
		if didErr != nil {
			return nil, fmt.Errorf("failed to resolve repo DID for %s/%s: %w", ownerDid, repoName, didErr)
		}

		var lookupErr error
		repoPath, _, _, lookupErr = h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
		if lookupErr != nil {
			return nil, fmt.Errorf("failed to resolve repo on disk: %w", lookupErr)
		}

	default:
		return nil, fmt.Errorf("ignoring pull record: target has neither repo nor repoDid")
	}

	_, err := git.Open(repoPath, record.Source.Branch)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repository: %w", err)
	}

	return &targetRepo{
		RepoPath: repoPath,
		OwnerDid: ownerDid,
		RepoName: repoName,
		RepoDid:  repoDid,
	}, nil
}

func (h *Knot) fetchLatestSubmission(ctx context.Context, did, rkey string, record *tangled.RepoPull) (*models.PullSubmission, error) {
	// resolve the PR owner's identity to fetch the blob from their PDS
	prOwnerIdent, err := h.resolver.ResolveIdent(ctx, did)
	if err != nil || prOwnerIdent.Handle.IsInvalidHandle() {
		return nil, fmt.Errorf("failed to resolve PR owner handle: %w", err)
	}

	roundNumber := len(record.Rounds) - 1
	round := record.Rounds[roundNumber]

	// fetch the blob from the PR owner's PDS
	prOwnerPds := prOwnerIdent.PDSEndpoint()
	blobUrl, err := url.Parse(fmt.Sprintf("%s/xrpc/com.atproto.sync.getBlob", prOwnerPds))
	if err != nil {
		return nil, fmt.Errorf("failed to construct blob URL: %w", err)
	}
	q := blobUrl.Query()
	q.Set("cid", round.PatchBlob.Ref.String())
	q.Set("did", did)
	blobUrl.RawQuery = q.Encode()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, blobUrl.String(), nil)
	if err != nil {
		return nil, fmt.Errorf("failed to create blob request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	blobResp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("failed to fetch blob: %w", err)
	}
	defer blobResp.Body.Close()

	blob := io.ReadCloser(blobResp.Body)
	latestSubmission, err := models.PullSubmissionFromRecord(did, rkey, roundNumber, round, &blob)
	if err != nil {
		return nil, fmt.Errorf("failed to parse submission: %w", err)
	}

	return latestSubmission, nil
}

func (h *Knot) discoverWorkflows(ctx context.Context, repoPath, sha string) (workflow.RawPipeline, error) {
	gr, err := git.Open(repoPath, sha)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repository: %w", err)
	}

	workflowDir, err := gr.FileTree(ctx, workflow.WorkflowDir)
	if err != nil {
		return nil, fmt.Errorf("failed to open workflow directory: %w", err)
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

	return pipeline, nil
}

func (h *Knot) compilePipeline(ctx context.Context, targetRepo *targetRepo, sourceBranch, sourceSha, targetBranch string, rawPipeline workflow.RawPipeline) tangled.Pipeline {
	l := log.FromContext(ctx)

	trigger := tangled.Pipeline_PullRequestTriggerData{
		Action:       "create",
		SourceBranch: sourceBranch,
		SourceSha:    sourceSha,
		TargetBranch: targetBranch,
	}

	compiler := workflow.Compiler{
		Trigger: tangled.Pipeline_TriggerMetadata{
			Kind:        string(workflow.TriggerKindPullRequest),
			PullRequest: &trigger,
			Repo: &tangled.Pipeline_TriggerRepo{
				Knot:    h.c.Server.Hostname,
				RepoDid: &targetRepo.RepoDid,
				Did:     targetRepo.OwnerDid,
				Repo:    &targetRepo.RepoName,
			},
		},
	}

	l.Info("raw", "raw", rawPipeline)
	parsed := compiler.Parse(rawPipeline)
	l.Info("parsed", "parsed", parsed)
	compiled := compiler.Compile(parsed)

	l.Info("compiler diagnostics", "diagnostics", compiler.Diagnostics)

	return compiled
}

func (h *Knot) processPull(ctx context.Context, event *jmodels.Event) error {
	raw := json.RawMessage(event.Commit.Record)
	rkey := event.Commit.RKey
	did := event.Did

	var record tangled.RepoPull
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal record: %w", err)
	}

	l := log.FromContext(ctx)
	l = l.With("handler", "processPull")
	l = l.With("did", did)

	l.Info("validating pull record")
	targetRepo, err := h.validatePullRecord(ctx, &record)
	if err != nil {
		l.Warn("pull record did not validate, skipping...")
		return err
	}

	l = l.With("target_repo", record.Target.Repo)
	l = l.With("target_branch", record.Target.Branch)

	l.Info("fetching latest submission")
	latestSubmission, err := h.fetchLatestSubmission(ctx, did, rkey, &record)
	if err != nil {
		return err
	}

	sha := latestSubmission.SourceRev
	if sha == "" {
		return fmt.Errorf("failed to extract source SHA from pull submission")
	}
	l = l.With("sha", sha)

	l.Info("discovering workflows", "repo_path", targetRepo.RepoPath)
	pipeline, err := h.discoverWorkflows(ctx, targetRepo.RepoPath, sha)
	if err != nil {
		return err
	}

	l.Info("compiling pipeline", "workflow_count", len(pipeline))
	cp := h.compilePipeline(ctx, targetRepo, record.Source.Branch, sha, record.Target.Branch, pipeline)

	// do not run empty pipelines
	if cp.Workflows == nil {
		l.Info("skipping empty pipeline")
		return nil
	}

	l.Info("marshaling pipeline event")
	eventJson, err := json.Marshal(cp)
	if err != nil {
		return fmt.Errorf("failed to marshal pipeline event: %w", err)
	}

	ev := db.Event{
		Rkey:      TID(),
		Nsid:      tangled.PipelineNSID,
		EventJson: string(eventJson),
	}

	l.Info("inserting pipeline event")
	return h.db.InsertEvent(ev, h.n)
}

// duplicated from add collaborator
func (h *Knot) processCollaborator(ctx context.Context, event *jmodels.Event) error {
	raw := json.RawMessage(event.Commit.Record)
	did := event.Did

	var record tangled.RepoCollaborator
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal record: %w", err)
	}

	subjectId, err := h.resolver.ResolveIdent(ctx, record.Subject)
	if err != nil || subjectId.Handle.IsInvalidHandle() {
		return err
	}

	var rbacResource string
	switch {
	case record.RepoDid != nil && *record.RepoDid != "":
		ownerDid, _, lookupErr := h.db.GetRepoKeyOwner(*record.RepoDid)
		if lookupErr != nil {
			return fmt.Errorf("unknown repo DID %s: %w", *record.RepoDid, lookupErr)
		}
		if ownerDid != did {
			return fmt.Errorf("collaborator record author %s does not own repo %s", did, *record.RepoDid)
		}
		rbacResource = *record.RepoDid

	case record.Repo != nil:
		// TODO: get rid of this PDS fetch once all repos have DIDs
		repoAt, parseErr := syntax.ParseATURI(*record.Repo)
		if parseErr != nil {
			return parseErr
		}

		owner, resolveErr := h.resolver.ResolveIdent(ctx, repoAt.Authority().String())
		if resolveErr != nil || owner.Handle.IsInvalidHandle() {
			return fmt.Errorf("failed to resolve handle: %w", resolveErr)
		}

		xrpcc := xrpc.Client{
			Host: owner.PDSEndpoint(),
		}

		resp, getErr := comatproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
		if getErr != nil {
			return getErr
		}

		repo := resp.Value.Val.(*tangled.Repo)
		repoDid, didErr := h.db.GetRepoDid(owner.DID.String(), repo.Name)
		if didErr != nil {
			return fmt.Errorf("failed to resolve repo DID for %s/%s: %w", owner.DID.String(), repo.Name, didErr)
		}
		rbacResource = repoDid

	default:
		return fmt.Errorf("collaborator record has neither repo nor repoDid")
	}

	ok, err := h.e.IsCollaboratorInviteAllowed(did, rbac.ThisServer, rbacResource)
	if err != nil {
		return fmt.Errorf("failed to check permissions: %w", err)
	}
	if !ok {
		return fmt.Errorf("insufficient permissions: %s, %s, %s", did, "IsCollaboratorInviteAllowed", rbacResource)
	}

	if err := h.db.AddDid(subjectId.DID.String()); err != nil {
		return err
	}
	h.jc.AddDid(subjectId.DID.String())

	if err := h.e.AddCollaborator(subjectId.DID.String(), rbac.ThisServer, rbacResource); err != nil {
		return err
	}

	return h.fetchAndAddKeys(ctx, subjectId.DID.String())
}

func (h *Knot) fetchAndAddKeys(ctx context.Context, did string) error {
	l := log.FromContext(ctx)

	keysEndpoint, err := url.JoinPath(h.c.AppViewEndpoint, "keys", did)
	if err != nil {
		l.Error("error building endpoint url", "did", did, "error", err.Error())
		return fmt.Errorf("error building endpoint url: %w", err)
	}

	resp, err := http.Get(keysEndpoint)
	if err != nil {
		l.Error("error getting keys", "did", did, "error", err)
		return fmt.Errorf("error getting keys: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode == http.StatusNotFound {
		l.Info("no keys found for did", "did", did)
		return nil
	}

	plaintext, err := io.ReadAll(resp.Body)
	if err != nil {
		l.Error("error reading response body", "error", err)
		return fmt.Errorf("error reading response body: %w", err)
	}

	for key := range strings.SplitSeq(string(plaintext), "\n") {
		if key == "" {
			continue
		}
		pk := db.PublicKey{
			Did: did,
		}
		pk.Key = key
		if err := h.db.AddPublicKey(pk); err != nil {
			l.Error("failed to add public key", "error", err)
			return fmt.Errorf("failed to add public key: %w", err)
		}
	}
	return nil
}

func (h *Knot) processMessages(ctx context.Context, event *jmodels.Event) error {
	var err error
	switch event.Kind {
	case jmodels.EventKindIdentity:
		err = h.resolver.InvalidateIdent(ctx, event.Did)
	case jmodels.EventKindCommit:
		switch event.Commit.Collection {
		case tangled.PublicKeyNSID:
			err = h.processPublicKey(ctx, event)
		case tangled.KnotMemberNSID:
			err = h.processKnotMember(ctx, event)
		case tangled.RepoPullNSID:
			err = h.processPull(ctx, event)
		case tangled.RepoCollaboratorNSID:
			err = h.processCollaborator(ctx, event)
		}
	default:
		return nil
	}

	if err != nil {
		args := []any{"kind", event.Kind, "err", err}
		if event.Kind == jmodels.EventKindCommit {
			args = append(args, "nsid", event.Commit.Collection)
		}
		h.l.Warn("failed to process event, skipping", args...)
	}

	lastTimeUs := event.TimeUS + 1
	if saveErr := h.db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
		h.l.Error("failed to save cursor", "err", saveErr)
	}

	return nil
}
