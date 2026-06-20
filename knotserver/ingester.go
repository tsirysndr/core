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
	"tangled.org/core/eventstream"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/git"
	knotxrpc "tangled.org/core/knotserver/xrpc"
	"tangled.org/core/log"
	"tangled.org/core/tid"
	"tangled.org/core/workflow"
)

func (h *Knot) processPublicKey(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx).With("handler", "processPublicKey", "did", event.Did, "rkey", event.Commit.RKey)
	did := syntax.DID(event.Did)
	rkey := syntax.RecordKey(event.Commit.RKey)

	switch event.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		var record tangled.PublicKey
		if err := json.Unmarshal(json.RawMessage(event.Commit.Record), &record); err != nil {
			return fmt.Errorf("failed to unmarshal record: %w", err)
		}

		pk := db.PublicKey{
			Did:       did,
			Rkey:      rkey,
			PublicKey: record,
		}
		if err := h.db.UpsertPublicKey(pk); err != nil {
			return fmt.Errorf("failed to upsert public key: %w", err)
		}
		l.Info("upserted public key from firehose")
	case jmodels.CommitOperationDelete:
		if err := h.db.DeletePublicKeyByRkey(did, rkey); err != nil {
			return fmt.Errorf("failed to delete public key: %w", err)
		}
		l.Info("deleted public key from firehose")
	}

	return nil
}

// returns a repo path on disk if present, and error if not
type targetRepo struct {
	RepoPath      string
	OwnerDid      string
	RepoName      string
	RepoDid       string
	DefaultBranch string // default branch
}

func (h *Knot) validatePullRecord(ctx context.Context, record *tangled.RepoPull) (*targetRepo, error) {
	if record.Target == nil {
		return nil, fmt.Errorf("ignoring pull record: target repo is nil")
	}

	l := log.FromContext(ctx).With("handler", "validatePullRecord")
	l = l.With("target_repo", record.Target.Repo)
	l = l.With("target_branch", record.Target.Branch)

	if record.Source == nil {
		return nil, fmt.Errorf("ignoring pull record: not a branch-based pull request")
	}

	if record.Source.Repo != nil {
		return nil, fmt.Errorf("ignoring pull record: fork based pull")
	}

	var repoPath, ownerDid, repoName, repoDid string
	switch {
	case strings.HasPrefix(record.Target.Repo, "did:"):
		repoDid = record.Target.Repo
		var lookupErr error
		repoPath, ownerDid, repoName, lookupErr = h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, repoDid)
		if lookupErr != nil {
			return nil, fmt.Errorf("unknown target repo DID %s: %w", repoDid, lookupErr)
		}

	case strings.Contains(record.Target.Repo, "/"):
		// TODO: get rid of this PDS fetch once all repos have DIDs
		repoAt, parseErr := syntax.ParseATURI(record.Target.Repo)
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

		repo, ok := resp.Value.Val.(*tangled.Repo)
		if !ok {
			return nil, fmt.Errorf("record at %s is not a tangled.Repo", repoAt)
		}

		if repo.Knot != h.c.Server.Hostname {
			return nil, fmt.Errorf("rejected pull record: not this knot, %s != %s", repo.Knot, h.c.Server.Hostname)
		}

		ownerDid = ident.DID.String()
		repoName = repoAt.RecordKey().String()

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
		return nil, fmt.Errorf("ignoring pull record: target repo has unrecognized format: %s", record.Target.Repo)
	}

	gr, err := git.Open(repoPath, record.Source.Branch)
	if err != nil {
		return nil, fmt.Errorf("failed to open git repository: %w", err)
	}

	defaultBranch, _ := gr.FindMainBranch()

	return &targetRepo{
		RepoPath:      repoPath,
		OwnerDid:      ownerDid,
		RepoName:      repoName,
		RepoDid:       repoDid,
		DefaultBranch: defaultBranch,
	}, nil
}

func (h *Knot) fetchLatestSubmission(ctx context.Context, did, rkey string, record *tangled.RepoPull) (*models.PullSubmission, error) {
	// resolve the PR owner's identity to fetch the blob from their PDS
	prOwnerIdent, err := h.resolver.ResolveIdent(ctx, did)
	if err != nil || prOwnerIdent.Handle.IsInvalidHandle() {
		return nil, fmt.Errorf("failed to resolve PR owner handle: %w", err)
	}

	if len(record.Rounds) == 0 {
		return nil, fmt.Errorf("failed to fetch latest submission, no rounds in record")
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
				Knot:          h.c.Server.Hostname,
				RepoDid:       &targetRepo.RepoDid,
				Did:           targetRepo.OwnerDid,
				Repo:          &targetRepo.RepoName,
				DefaultBranch: targetRepo.DefaultBranch,
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

	ev := eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      tangled.PipelineNSID,
		EventJson: eventJson,
	}

	l.Info("inserting pipeline event")
	return h.db.InsertEvent(ev, h.n)
}

func (h *Knot) processRepo(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx).With("handler", "processRepo", "did", event.Did, "rkey", event.Commit.RKey)

	rkey := strings.TrimSuffix(strings.TrimSpace(event.Commit.RKey), ".git")
	if rkey == "" {
		return nil
	}

	if event.Commit.Operation == jmodels.CommitOperationDelete {
		return nil
	}

	if event.Commit.Operation != jmodels.CommitOperationCreate && event.Commit.Operation != jmodels.CommitOperationUpdate {
		return nil
	}

	raw := json.RawMessage(event.Commit.Record)
	var record tangled.Repo
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal repo record: %w", err)
	}

	if record.Knot != h.c.Server.Hostname {
		return nil
	}
	if record.RepoDid == nil || *record.RepoDid == "" {
		l.Info("skipping repo event without repoDid")
		return nil
	}
	repoDid := *record.RepoDid

	if err := knotxrpc.ValidateRepoName(rkey); err != nil {
		l.Warn("skipping repo event with invalid rkey", "repoDid", repoDid, "rkey", rkey, "err", err)
		return nil
	}

	ownerDid, _, lookupErr := h.db.GetRepoKeyOwner(repoDid)
	if lookupErr != nil {
		l.Info("skipping repo event for unknown repoDid", "repoDid", repoDid)
		return nil
	}
	if ownerDid != event.Did {
		l.Warn("repo event author does not own repoDid", "repoDid", repoDid, "author", event.Did)
		return nil
	}

	alias := db.RepoAlias{
		OwnerDid: event.Did,
		Rkey:     rkey,
		RepoDid:  repoDid,
		Rev:      event.Commit.Rev,
	}
	if err := h.db.UpsertRepoAlias(alias); err != nil {
		l.Warn("failed to upsert repo alias", "err", err)
		return nil
	}

	l.Info("recorded repo alias", "repoDid", repoDid, "rkey", rkey, "rev", event.Commit.Rev)
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
		case tangled.RepoNSID:
			err = h.processRepo(ctx, event)
		case tangled.RepoPullNSID:
			err = h.processPull(ctx, event)
		}
	default:
		return nil
	}

	if err != nil {
		args := []any{"kind", event.Kind, "err", err}
		if event.Kind == jmodels.EventKindCommit {
			args = append(args, "nsid", event.Commit.Collection, "did", event.Did, "rkey", event.Commit.RKey)
		}
		h.l.Warn("failed to process event, skipping", args...)
	}

	lastTimeUs := event.TimeUS + 1
	if saveErr := h.db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
		h.l.Error("failed to save cursor", "err", saveErr)
	}

	return nil
}
