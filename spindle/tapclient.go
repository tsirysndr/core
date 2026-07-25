package spindle

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	avmodels "tangled.org/core/appview/models"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/log"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/git"
	"tangled.org/core/spindle/models"
	"tangled.org/core/tapc"
	"tangled.org/core/tid"
	"tangled.org/core/workflow"
)

const (
	maxPendingPerRepo = 64
	pendingCollabTTL  = 10 * time.Minute
)

type pendingCollabEvent struct {
	evt *tapc.RecordEventData
	at  time.Time
}

type Tap struct {
	logger         *slog.Logger
	spindle        *Spindle
	tap            tapc.Client
	pendingMu      sync.Mutex
	pendingCollabs map[syntax.DID][]pendingCollabEvent
}

func NewTapClient(s *Spindle) *Tap {
	return &Tap{
		logger:         log.SubLogger(s.l, "tapclient"),
		spindle:        s,
		tap:            tapc.NewClient(s.cfg.Server.Tap.Url, s.cfg.Server.Tap.AdminPassword),
		pendingCollabs: make(map[syntax.DID][]pendingCollabEvent),
	}
}

func (t *Tap) AddOwnerDIDs(ctx context.Context, dids []syntax.DID) error {
	if len(dids) == 0 {
		return nil
	}
	return t.tap.AddRepos(ctx, dids)
}

func (t *Tap) Start(connCtx context.Context) {
	go t.tap.Connect(connCtx, &tapc.SimpleIndexer{
		EventHandler:   t.processEvent,
		ConnectHandler: t.onConnect,
	})
	go t.purgePendingCollabsLoop(t.spindle.rootCtx)
}

func (t *Tap) onConnect(ctx context.Context) {
	t.spindle.declareTapInterest(ctx)
}

func (t *Tap) processEvent(ctx context.Context, evt tapc.Event) error {
	if evt.Type != tapc.EvtRecord || evt.Record == nil {
		return nil
	}
	switch evt.Record.Collection.String() {
	case tangled.RepoNSID:
		return t.processRepo(ctx, evt.Record)
	case tangled.RepoCollaboratorNSID:
		return t.processCollaborator(ctx, evt.Record)
	}
	return nil
}

func (t *Tap) processRepo(ctx context.Context, evt *tapc.RecordEventData) error {
	l := t.logger.With("collection", tangled.RepoNSID, "did", evt.Did, "rkey", evt.Rkey)

	ownerDid := evt.Did
	rkey := evt.Rkey

	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.Repo{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			l.Warn("skipping invalid repo record", "err", err)
			return nil
		}

		hostname := t.spindle.cfg.Server.Hostname
		prior, priorErr := t.spindle.db.GetRepoByOwnerRkey(ownerDid, rkey)
		knownRepo := priorErr == nil

		if record.Spindle == nil || *record.Spindle != hostname {
			if knownRepo {
				l.Info("tearing down repo reassigned from this spindle", "newSpindle", record.Spindle)
				return t.teardownRepo(l, prior, ownerDid, rkey)
			}
			return nil
		}

		if record.RepoDid == nil || *record.RepoDid == "" {
			l.Warn("skipping repo record without repoDid")
			return nil
		}
		repoDid, err := syntax.ParseDID(*record.RepoDid)
		if err != nil {
			l.Warn("skipping repo record with malformed repoDid", "value", *record.RepoDid, "err", err)
			return nil
		}

		if err := t.spindle.e.AddRepo(ownerDid.String(), rbac.ThisServer, repoDid.String()); err != nil {
			l.Error("failed to add repo policy", "err", err)
			return fmt.Errorf("add repo policy: %w", err)
		}

		src := eventconsumer.NewKnotSource(record.Knot)
		t.spindle.ks.AddSource(t.spindle.rootCtx, src)

		repo := db.Repo{
			Knot:      record.Knot,
			Owner:     ownerDid,
			Rkey:      rkey,
			RepoDid:   repoDid,
			CreatedAt: record.CreatedAt,
		}

		if err := t.spindle.db.AddRepo(repo); err != nil {
			l.Error("failed to add repo row", "err", err)
			return fmt.Errorf("add repo: %w", err)
		}

		// setup sparse sync
		repoCloneUri := t.spindle.newRepoCloneUrl(repo.Knot, repo.RepoDid)
		repoPath := t.spindle.newRepoPath(repo.RepoDid)
		if err := git.SparseSyncGitRepo(ctx, repoCloneUri, repoPath, ""); err != nil {
			return fmt.Errorf("setting up sparse-clone git repo: %w", err)
		}

		legacyName := ""
		if record.Name != nil {
			legacyName = *record.Name
		}
		migrateLegacyRepoSecrets(ctx, t.spindle.db, t.spindle.vault, l, ownerDid, legacyName, rkey, repoDid)
		migrateLegacyRepoCasbin(ctx, t.spindle.db, t.spindle.e, l, ownerDid, legacyName, rkey, repoDid)

		if removed, err := t.spindle.db.CollapseRepoSiblings(ownerDid, repoDid); err != nil {
			l.Warn("collapse rename siblings failed", "err", err)
		} else if removed > 0 {
			l.Info("collapsed rename leftovers", "owner", ownerDid, "repo_did", repoDid, "removed", removed)
		}

		if e := t.spindle.embedTap; e == nil || !e.closed.Load() {
			if err := t.tap.AddRepos(ctx, []syntax.DID{ownerDid}); err != nil {
				l.Warn("tap AddRepos rejected", "did", ownerDid, "err", err)
			}
		}
		t.spindle.jc.AddDid(ownerDid.String())

		t.drainPendingCollabs(ctx, repoDid)

	case tapc.RecordDeleteAction:
		repo, err := t.spindle.db.GetRepoByOwnerRkey(ownerDid, rkey)
		if err != nil {
			l.Info("skipping delete for unknown repo")
			return nil
		}
		return t.teardownRepo(l, repo, ownerDid, rkey)
	}
	return nil
}

func (t *Tap) teardownRepo(l *slog.Logger, repo *db.Repo, ownerDid syntax.DID, rkey syntax.RecordKey) error {
	if repo.RepoDid != "" {
		collabs, err := t.spindle.db.ListCollaboratorsByRepoDid(repo.RepoDid)
		if err != nil {
			l.Error("failed to list collaborators for cleanup", "err", err)
			return fmt.Errorf("list collaborators: %w", err)
		}
		for _, c := range collabs {
			if err := t.spindle.e.RemoveCollaborator(c.Subject.String(), rbac.ThisServer, repo.RepoDid.String()); err != nil {
				l.Error("failed to remove collaborator policy", "subject", c.Subject, "err", err)
				return fmt.Errorf("remove collaborator policy: %w", err)
			}
		}
		if err := t.spindle.db.DeleteRepoCollaboratorsByRepoDid(repo.RepoDid); err != nil {
			l.Error("failed to clear collaborator rows", "err", err)
			return err
		}
		if err := t.spindle.e.RemoveRepo(ownerDid.String(), rbac.ThisServer, repo.RepoDid.String()); err != nil {
			l.Error("failed to remove repo policy", "err", err)
			return fmt.Errorf("remove repo policy: %w", err)
		}
	}
	if err := t.spindle.db.DeleteRepoByOwnerRkey(ownerDid, rkey); err != nil {
		l.Error("failed to delete repo row", "err", err)
		return fmt.Errorf("delete repo row: %w", err)
	}
	// TODO: clear sparse-synced git repo
	return nil
}

func (t *Tap) processCollaborator(ctx context.Context, evt *tapc.RecordEventData) error {
	l := t.logger.With("collection", tangled.RepoCollaboratorNSID, "did", evt.Did, "rkey", evt.Rkey)

	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.RepoCollaborator{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			l.Warn("skipping invalid collaborator record", "err", err)
			return nil
		}

		actor := evt.Did
		rkey := evt.Rkey

		subjectDid, err := syntax.ParseDID(record.Subject)
		if err != nil {
			l.Info("skipping collaborator with malformed subject DID", "subject", record.Subject, "err", err)
			return nil
		}
		if _, err := t.spindle.res.ResolveIdent(ctx, subjectDid.String()); err != nil {
			l.Info("skipping unresolvable collaborator subject", "subject", subjectDid, "err", err)
			return nil
		}

		repoRefDid, err := syntax.ParseDID(record.Repo)
		if err != nil {
			l.Info("skipping collaborator with non-DID repo ref", "repo", record.Repo, "err", err)
			return nil
		}
		repo, lookupErr := t.spindle.db.GetRepoByDid(repoRefDid)
		if errors.Is(lookupErr, sql.ErrNoRows) {
			t.bufferCollab(repoRefDid, evt)
			l.Info("buffering collaborator until repo arrives", "repo", repoRefDid)
			return nil
		}
		if lookupErr != nil {
			return fmt.Errorf("lookup repo %s: %w", repoRefDid, lookupErr)
		}
		repoDid := repo.RepoDid
		ownerDid := repo.Owner

		if actor != ownerDid {
			l.Info("rejecting collaborator with non-owner actor", "actor", actor, "owner", ownerDid)
			return nil
		}

		ok, err := t.spindle.e.IsCollaboratorInviteAllowed(ownerDid.String(), rbac.ThisServer, repoDid.String())
		if err != nil {
			l.Error("invite permission check failed", "err", err)
			return fmt.Errorf("invite check: %w", err)
		}
		if !ok {
			l.Info("rejecting collaborator invite", "owner", ownerDid, "repo", repoDid)
			return nil
		}

		prior, priorErr := t.spindle.db.GetRepoCollaborator(actor, rkey)
		staleSubject := priorErr == nil && (prior.Subject != subjectDid || prior.RepoDid != repoDid)

		if err := t.spindle.e.AddCollaborator(subjectDid.String(), rbac.ThisServer, repoDid.String()); err != nil {
			l.Error("failed to add collaborator policy", "err", err)
			return fmt.Errorf("add collaborator policy: %w", err)
		}
		if staleSubject {
			if err := t.spindle.e.RemoveCollaborator(prior.Subject.String(), rbac.ThisServer, prior.RepoDid.String()); err != nil {
				l.Error("failed to remove stale collaborator policy", "err", err)
				return fmt.Errorf("remove stale collaborator: %w", err)
			}
		}
		if err := t.spindle.db.AddRepoCollaborator(db.RepoCollaborator{
			OwnerDid: actor,
			Rkey:     rkey,
			Subject:  subjectDid,
			RepoDid:  repoDid,
		}); err != nil {
			l.Error("failed to persist collaborator row", "err", err)
			return fmt.Errorf("track collaborator: %w", err)
		}

	case tapc.RecordDeleteAction:
		actor := evt.Did
		rkey := evt.Rkey

		tracked, err := t.spindle.db.GetRepoCollaborator(actor, rkey)
		if err != nil {
			l.Info("skipping delete for unknown collaborator record")
			return nil
		}
		if err := t.spindle.e.RemoveCollaborator(tracked.Subject.String(), rbac.ThisServer, tracked.RepoDid.String()); err != nil {
			l.Error("failed to remove collaborator policy", "err", err)
			return fmt.Errorf("remove collaborator policy: %w", err)
		}
		if err := t.spindle.db.DeleteRepoCollaborator(actor, rkey); err != nil {
			l.Error("failed to delete collaborator row", "err", err)
			return fmt.Errorf("delete collaborator row: %w", err)
		}
	}
	return nil
}

func (s *Spindle) processPull(ctx context.Context, evt *tapc.RecordEventData) error {
	l := s.l.With("component", "ingester", "collection", evt.Collection, "did", evt.Did, "rkey", evt.Rkey)

	// only listen to live events
	if !evt.Live {
		l.Info("skipping backfill event", "event", evt.AtUri())
		return nil
	}

	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.RepoPull{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			l.Error("invalid record", "err", err)
			return fmt.Errorf("parsing record: %w", err)
		}

		// ignore legacy records
		if record.Target == nil {
			l.Info("ignoring pull record: target repo is nil")
			return nil
		}

		// ignore patch-based and fork-based PRs
		if record.Source == nil || record.Source.Repo != nil {
			l.Info("ignoring pull record: not a branch-based pull request")
			return nil
		}

		// skip if target repo is unknown
		repo, err := s.db.GetRepoByDid(syntax.DID(record.Target.Repo))
		if err != nil {
			l.Warn("target repo is not ingested yet", "repo", record.Target.Repo, "err", err)
			return fmt.Errorf("target repo is unknown")
		}

		// only accept branch-based PR (excluding patch-based and fork-based)
		if record.Source == nil || record.Source.Repo != nil {
			l.Warn("skipping non-branch-based PR")
			return nil
		}

		// check if pull record author has push access to target repo
		allowed, err := s.e.IsPushAllowed(evt.Did.String(), rbac.ThisServer, repo.RepoDid.String())
		if err != nil {
			return fmt.Errorf("checking push access for pull record author: %w", err)
		}
		if !allowed {
			l.Warn("rejecting pull-triggered pipeline. author has no push access",
				"author", evt.Did, "repo", repo.RepoDid)
			return nil
		}

		latestSubmission, err := s.fetchLatestSubmission(ctx, evt.Did.String(), evt.Rkey.String(), &record)
		if err != nil {
			return err
		}
		sourceSha := latestSubmission.SourceRev

		scheme := "https"
		if s.cfg.Server.Dev {
			scheme = "http"
		}
		client := &indigoxrpc.Client{Host: fmt.Sprintf("%s://%s", scheme, repo.Knot)}

		// fetch current default branch
		defaultBranch, _ := func(repo syntax.DID) (string, error) {
			defaultBranchOut, err := tangled.RepoGetDefaultBranch(ctx, client, repo.String())
			if err != nil {
				return "", err
			}
			return defaultBranchOut.Name, nil
		}(repo.RepoDid)

		compiler := workflow.Compiler{
			Trigger: tangled.Pipeline_TriggerMetadata{
				Kind: string(workflow.TriggerKindPullRequest),
				PullRequest: &tangled.Pipeline_PullRequestTriggerData{
					SourceBranch: record.Source.Branch,
					SourceSha:    sourceSha,
					TargetBranch: record.Target.Branch,
				},
				Repo: &tangled.Pipeline_TriggerRepo{
					Did:           repo.Owner.String(),
					Knot:          repo.Knot,
					Repo:          (*string)(&repo.Rkey),
					RepoDid:       (*string)(&repo.RepoDid),
					DefaultBranch: defaultBranch,
				},
			},
		}

		repoUri := s.newRepoCloneUrl(repo.Knot, repo.RepoDid)
		repoPath := s.newRepoPath(repo.RepoDid)

		// load workflow definitions from rev (without spindle context)
		rawPipeline, err := s.loadPipeline(ctx, repoUri, repoPath, sourceSha)
		if err != nil {
			// don't retry
			l.Error("failed loading pipeline", "err", err)
			return nil
		}
		if len(rawPipeline) == 0 {
			l.Info("no workflow definition find for the repo. skipping the event")
			return nil
		}
		tpl := compiler.Compile(compiler.Parse(rawPipeline))
		// TODO: pass compile error to workflow log
		for _, w := range compiler.Diagnostics.Errors {
			l.Error(w.String())
		}
		for _, w := range compiler.Diagnostics.Warnings {
			l.Warn(w.String())
		}
		if len(tpl.Workflows) == 0 {
			l.Info("no workflow matching trigger 'pull_request'. skipping the event")
			return nil
		}

		pipelineId := models.PipelineId{
			Knot: tpl.TriggerMetadata.Repo.Knot,
			Rkey: tid.TID(),
		}
		if err := s.db.CreatePipelineEvent(pipelineId.Rkey, tpl, s.n); err != nil {
			l.Error("failed to create pipeline event", "err", err)
			return nil
		}
		sourceRepo, err := s.resolvePipelineSourceRepo(ctx, tpl.TriggerMetadata)
		if err != nil {
			l.Error("failed resolving pipeline source repo", "err", err)
			return nil
		}
		err = s.processPipeline(repo.RepoDid, tpl, pipelineId, sourceRepo)
		if err != nil {
			// don't retry
			l.Error("failed processing pipeline", "err", err)
			return nil
		}
	case tapc.RecordDeleteAction:
		// no-op
	}
	return nil
}

func (t *Tap) bufferCollab(repoDid syntax.DID, evt *tapc.RecordEventData) {
	t.pendingMu.Lock()
	defer t.pendingMu.Unlock()
	list := t.pendingCollabs[repoDid]
	list = append(list, pendingCollabEvent{evt: evt, at: time.Now()})
	if len(list) > maxPendingPerRepo {
		list = list[len(list)-maxPendingPerRepo:]
	}
	t.pendingCollabs[repoDid] = list
}

func (t *Tap) drainPendingCollabs(ctx context.Context, repoDid syntax.DID) {
	t.pendingMu.Lock()
	list := t.pendingCollabs[repoDid]
	delete(t.pendingCollabs, repoDid)
	t.pendingMu.Unlock()
	if len(list) == 0 {
		return
	}
	cutoff := time.Now().Add(-pendingCollabTTL)
	for _, p := range list {
		if p.at.Before(cutoff) {
			continue
		}
		if err := t.processCollaborator(ctx, p.evt); err != nil {
			t.logger.Warn("replaying buffered collaborator failed", "repo", repoDid, "rkey", p.evt.Rkey, "err", err)
		}
	}
}

func (t *Tap) purgePendingCollabsLoop(ctx context.Context) {
	ticker := time.NewTicker(pendingCollabTTL / 2)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			t.purgeStalePendingCollabs()
		}
	}
}

func (t *Tap) purgeStalePendingCollabs() {
	cutoff := time.Now().Add(-pendingCollabTTL)
	t.pendingMu.Lock()
	defer t.pendingMu.Unlock()
	expired := 0
	for did, list := range t.pendingCollabs {
		kept := list[:0]
		for _, p := range list {
			if !p.at.Before(cutoff) {
				kept = append(kept, p)
			} else {
				expired++
			}
		}
		if len(kept) == 0 {
			delete(t.pendingCollabs, did)
		} else {
			t.pendingCollabs[did] = kept
		}
	}
	if expired > 0 {
		t.logger.Warn("expired buffered collaborator events without matching repo arrival", "count", expired, "ttl", pendingCollabTTL)
	}
}

func (s *Spindle) fetchLatestSubmission(ctx context.Context, did, rkey string, record *tangled.RepoPull) (*avmodels.PullSubmission, error) {
	// resolve the PR owner's identity to fetch the blob from their PDS
	prOwnerIdent, err := s.res.ResolveIdent(ctx, did)
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

	latestSubmission, err := avmodels.PullSubmissionFromRecord(did, rkey, roundNumber, round, blobResp.Body)
	if err != nil {
		return nil, fmt.Errorf("failed to parse submission: %w", err)
	}

	return latestSubmission, nil
}
