package db

import (
	"context"
	"encoding/json"
	"slices"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
)

func seedPipelineEvent(t *testing.T, d *DB, rkey, repoDid, kind string, created int64) {
	t.Helper()
	repo := repoDid
	tm := &tangled.Pipeline_TriggerMetadata{
		Kind: kind,
		Repo: &tangled.Pipeline_TriggerRepo{Knot: "knot.test", RepoDid: &repo, Did: repoDid},
	}
	switch kind {
	case "push":
		tm.Push = &tangled.Pipeline_PushTriggerData{NewSha: "sha-" + rkey, Ref: "refs/heads/main"}
	case "pull_request":
		tm.PullRequest = &tangled.Pipeline_PullRequestTriggerData{SourceSha: "sha-" + rkey, SourceBranch: "feature", TargetBranch: "main"}
	case "manual":
		tm.Manual = &tangled.Pipeline_ManualTriggerData{Sha: "sha-" + rkey}
	}
	raw := tangled.Pipeline{
		TriggerMetadata: tm,
		Workflows:       []*tangled.Pipeline_Workflow{{Name: "ci.yml"}},
	}
	eventJson, err := json.Marshal(raw)
	if err != nil {
		t.Fatalf("marshal pipeline: %v", err)
	}
	if _, err := d.Exec(
		`insert into events (rkey, nsid, event, created) values (?, 'sh.tangled.pipeline', ?, ?)`,
		rkey, string(eventJson), created,
	); err != nil {
		t.Fatalf("seed event %s: %v", rkey, err)
	}
}

func TestQueryPipelines_FilterByKind(t *testing.T) {
	d := newTestDB(t)
	ctx := context.Background()
	repo := "did:plc:boltless"
	base := time.Now().UnixNano()

	seedPipelineEvent(t, d, "p-push", repo, "push", base+1)
	seedPipelineEvent(t, d, "p-pull", repo, "pull_request", base+2)
	seedPipelineEvent(t, d, "p-manual", repo, "manual", base+3)

	cases := []struct {
		kinds     []string
		wantTotal int64
		wantKinds []string
	}{
		{nil, 3, []string{"manual", "pull_request", "push"}},
		{[]string{"push"}, 1, []string{"push"}},
		{[]string{"pull_request"}, 1, []string{"pull_request"}},
		{[]string{"manual"}, 1, []string{"manual"}},
		{[]string{"push", "pull_request"}, 2, []string{"pull_request", "push"}},
	}

	for _, tc := range cases {
		pipelines, _, total, err := d.QueryPipelines(ctx, repo, nil, "", tc.kinds, 30)
		if err != nil {
			t.Fatalf("kinds=%v: QueryPipelines: %v", tc.kinds, err)
		}
		if total != tc.wantTotal {
			t.Errorf("kinds=%v: total = %d, want %d", tc.kinds, total, tc.wantTotal)
		}
		var gotKinds []string
		for _, p := range pipelines {
			gotKinds = append(gotKinds, triggerKindOf(p))
		}
		slices.Sort(gotKinds)
		if !slices.Equal(gotKinds, tc.wantKinds) {
			t.Errorf("kinds=%v: returned %v, want %v", tc.kinds, gotKinds, tc.wantKinds)
		}
	}
}

func TestQueryPipelines_KindScopedToRepo(t *testing.T) {
	d := newTestDB(t)
	ctx := context.Background()
	base := time.Now().UnixNano()

	seedPipelineEvent(t, d, "a-push", "did:plc:alice", "push", base+1)
	seedPipelineEvent(t, d, "b-push", "did:plc:bob", "push", base+2)

	pipelines, _, total, err := d.QueryPipelines(ctx, "did:plc:alice", nil, "", []string{"push"}, 30)
	if err != nil {
		t.Fatalf("QueryPipelines: %v", err)
	}
	if total != 1 || len(pipelines) != 1 {
		t.Fatalf("total=%d len=%d, want exactly alice's single push pipeline", total, len(pipelines))
	}
	if pipelines[0].Repo == nil || *pipelines[0].Repo != "did:plc:alice" {
		t.Errorf("returned pipeline repo = %v, want did:plc:alice", pipelines[0].Repo)
	}
}

func triggerKindOf(p *tangled.CiPipeline) string {
	if p.Trigger == nil {
		return ""
	}
	switch {
	case p.Trigger.CiTrigger_Push != nil:
		return "push"
	case p.Trigger.CiTrigger_PullRequest != nil:
		return "pull_request"
	case p.Trigger.CiTrigger_Manual != nil:
		return "manual"
	}
	return ""
}
