package db

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
	spindle "tangled.org/core/spindle/models"
	"tangled.org/core/workflow"
)

// seedPipeline inserts a trigger + pipeline row and returns the pipeline.
func seedPipeline(t *testing.T, d *DB, knot, rkey, repoDid string) models.Pipeline {
	t.Helper()
	sha := strings.Repeat("a", 40)
	ref := "refs/heads/main"
	newSha := sha
	oldSha := strings.Repeat("0", 40)
	trigger := models.Trigger{
		Kind:       workflow.TriggerKindPush,
		PushRef:    &ref,
		PushNewSha: &newSha,
		PushOldSha: &oldSha,
	}
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	triggerID, err := AddTrigger(tx, trigger)
	if err != nil {
		tx.Rollback()
		t.Fatalf("AddTrigger: %v", err)
	}
	pipeline := models.Pipeline{
		Knot:      knot,
		Rkey:      rkey,
		RepoOwner: syntax.DID("did:plc:owner"),
		RepoName:  "repo",
		RepoDid:   repoDid,
		TriggerId: int(triggerID),
		Sha:       sha,
	}
	if err := AddPipeline(tx, pipeline); err != nil {
		tx.Rollback()
		t.Fatalf("AddPipeline: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return pipeline
}

// seedStatus inserts a pipeline_status row directly.
func seedStatus(t *testing.T, d *DB, spindleInstance, rkey, pipelineKnot, pipelineRkey, workflow string) {
	t.Helper()
	status := models.PipelineStatus{
		Spindle:      spindleInstance,
		Rkey:         rkey,
		PipelineKnot: pipelineKnot,
		PipelineRkey: pipelineRkey,
		Workflow:     workflow,
		Status:       spindle.StatusKindSuccess,
		Created:      time.Now(),
	}
	if err := AddPipelineStatus(context.Background(), d, status); err != nil {
		t.Fatalf("AddPipelineStatus: %v", err)
	}
}

// TestGetPipelineStatuses_SpindleValidation verifies that GetPipelineStatuses
// only returns statuses emitted by the spindle registered for the pipeline's
// repo, and silently drops statuses from a rogue spindle.
func TestGetPipelineStatuses_SpindleValidation(t *testing.T) {
	d := newTestDB(t)

	const (
		knot           = "knot.example.com"
		correctSpindle = "spindle.example.com"
		rogueSpindle   = "evil.example.com"
		repoDid        = "did:plc:testrepo"
		pipelineRkey   = "pipeline1"
	)

	// seed repo with the correct spindle
	repo := seedRepo(t, d, "did:plc:owner", knot, "repo", "repo", repoDid)
	if err := UpdateSpindle(d, repo.RepoDid, &[]string{correctSpindle}[0]); err != nil {
		t.Fatalf("UpdateSpindle: %v", err)
	}

	// seed the pipeline for this repo
	seedPipeline(t, d, knot, pipelineRkey, repoDid)

	// insert one status from the correct spindle, one from a rogue spindle
	seedStatus(t, d, correctSpindle, "status-valid", knot, pipelineRkey, "build")
	seedStatus(t, d, rogueSpindle, "status-rogue", knot, pipelineRkey, "build")

	pipelines, err := GetPipelineStatuses(d, 10, orm.FilterEq("p.repo_did", repoDid))
	if err != nil {
		t.Fatalf("GetPipelineStatuses: %v", err)
	}
	if len(pipelines) != 1 {
		t.Fatalf("expected 1 pipeline, got %d", len(pipelines))
	}

	statuses := pipelines[0].Statuses["build"].Data
	if len(statuses) != 1 {
		t.Fatalf("expected 1 status (from correct spindle), got %d", len(statuses))
	}
	if statuses[0].Spindle != correctSpindle {
		t.Errorf("expected spindle %q, got %q", correctSpindle, statuses[0].Spindle)
	}
}
