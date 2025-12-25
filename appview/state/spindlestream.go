package state

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pipelines"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventstream"
	"tangled.org/core/log"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	spindle "tangled.org/core/spindle/models"
	"tangled.org/core/workflow"
)

func Spindlestream(ctx context.Context, c *config.Config, d *db.DB, enforcer *rbac.Enforcer, pn *pipelines.StatusNotifier) (*ec.Consumer, error) {
	spindles, err := db.GetSpindles(ctx, d, orm.FilterIsNot("verified", "null"))
	if err != nil {
		return nil, err
	}

	hosts := make([]string, len(spindles))
	for i, s := range spindles {
		hosts[i] = s.Instance
	}

	return bootstrapStream(
		ctx, "spindlestream", ec.KindSpindle, hosts, c.Redis.Addr,
		c.Spindlestream,
		spindleIngester(d, pn),
	), nil
}

func spindleIngester(d *db.DB, pn *pipelines.StatusNotifier) ec.ProcessFunc {
	return func(ctx context.Context, source ec.Source, msg eventstream.Event) error {
		switch msg.Nsid {
		case tangled.PipelineNSID:
			return ingestPipeline(ctx, d, source, msg)
		case tangled.PipelineStatusNSID:
			return ingestPipelineStatus(ctx, d, pn, source, msg)
		}
		return nil
	}
}

func ingestPipeline(ctx context.Context, d *db.DB, source ec.Source, msg eventstream.Event) error {
	l := log.FromContext(ctx)

	var record tangled.Pipeline
	if err := json.Unmarshal(msg.EventJson, &record); err != nil {
		return fmt.Errorf("unmarshal pipeline: %w", err)
	}

	if record.TriggerMetadata == nil {
		return fmt.Errorf("empty trigger metadata: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	if record.TriggerMetadata.Repo == nil {
		return fmt.Errorf("empty repo: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	repoName := ""
	if record.TriggerMetadata.Repo.Repo != nil {
		repoName = *record.TriggerMetadata.Repo.Repo
	}

	repo, lookupErr := resolveRepo(d, record.TriggerMetadata.Repo.RepoDid, record.TriggerMetadata.Repo.Did, repoName)
	if lookupErr != nil {
		return fmt.Errorf("failed to look up repo: %w", lookupErr)
	}
	if repo.Spindle == "" {
		return fmt.Errorf("repo does not have a spindle configured yet: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	// trigger info
	var trigger models.Trigger
	var sha string
	trigger.Kind = workflow.TriggerKind(record.TriggerMetadata.Kind)
	switch trigger.Kind {
	case workflow.TriggerKindPush:
		trigger.PushRef = &record.TriggerMetadata.Push.Ref
		trigger.PushNewSha = &record.TriggerMetadata.Push.NewSha
		trigger.PushOldSha = &record.TriggerMetadata.Push.OldSha
		sha = *trigger.PushNewSha
	case workflow.TriggerKindPullRequest:
		trigger.PRSourceBranch = &record.TriggerMetadata.PullRequest.SourceBranch
		trigger.PRTargetBranch = &record.TriggerMetadata.PullRequest.TargetBranch
		trigger.PRSourceSha = &record.TriggerMetadata.PullRequest.SourceSha
		trigger.PRAction = &record.TriggerMetadata.PullRequest.Action
		sha = *trigger.PRSourceSha
	}

	tx, err := d.Begin()
	if err != nil {
		return fmt.Errorf("failed to start txn: %w", err)
	}

	triggerId, err := db.AddTrigger(tx, trigger)
	if err != nil {
		return fmt.Errorf("failed to add trigger entry: %w", err)
	}

	// TODO: we shouldn't even use knot to identify pipelines
	knot := record.TriggerMetadata.Repo.Knot
	pipeline := models.Pipeline{
		Rkey:      msg.Rkey,
		Knot:      knot,
		RepoOwner: syntax.DID(record.TriggerMetadata.Repo.Did),
		RepoName:  repoName,
		RepoDid:   repo.RepoDid,
		TriggerId: int(triggerId),
		Sha:       sha,
	}

	err = db.AddPipeline(tx, pipeline)
	if err != nil {
		return fmt.Errorf("failed to add pipeline: %w", err)
	}

	err = tx.Commit()
	if err != nil {
		return fmt.Errorf("failed to commit txn: %w", err)
	}

	l.Info("added pipeline", "pipeline", pipeline)

	return nil
}

func ingestPipelineStatus(ctx context.Context, d *db.DB, pn *pipelines.StatusNotifier, source ec.Source, msg eventstream.Event) error {
	var record tangled.PipelineStatus
	err := json.Unmarshal(msg.EventJson, &record)
	if err != nil {
		return err
	}

	pipelineUri, err := syntax.ParseATURI(record.Pipeline)
	if err != nil {
		return err
	}

	exitCode := 0
	if record.ExitCode != nil {
		exitCode = int(*record.ExitCode)
	}

	// pick the record creation time if possible, or use time.Now
	created := time.Now()
	if t, err := time.Parse(time.RFC3339, record.CreatedAt); err == nil && created.After(t) {
		created = t
	}

	status := models.PipelineStatus{
		Spindle:      source.Host,
		Rkey:         msg.Rkey,
		PipelineKnot: strings.TrimPrefix(pipelineUri.Authority().String(), "did:web:"),
		PipelineRkey: pipelineUri.RecordKey().String(),
		Created:      created,
		Workflow:     record.Workflow,
		Status:       spindle.StatusKind(record.Status),
		Error:        record.Error,
		ExitCode:     exitCode,
	}

	err = db.AddPipelineStatus(ctx, d, status)
	if err != nil {
		return fmt.Errorf("failed to add pipeline status: %w", err)
	}

	pn.Publish(pipelineUri)

	return nil
}
