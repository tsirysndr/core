package db

import (
	"context"
	"encoding/json"
	"strconv"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
	"tangled.org/core/workflow"
)

func (d *DB) QueryPipelines(ctx context.Context, repoDid string, commits []string, cursor string, kinds []string, limit int) ([]*tangled.CiPipeline, string, int64, error) {
	if limit <= 0 {
		limit = 30
	}

	var query string
	var args []any
	query = `
		select
			rkey, event, created from events
		where
			nsid = 'sh.tangled.pipeline'
			and coalesce(json_extract(event, '$.triggerMetadata.repo.repoDid'), json_extract(event, '$.triggerMetadata.repo.did')) = ?
	`
	args = append(args, repoDid)

	if len(commits) > 0 {
		placeholders := make([]string, len(commits))
		for i := range commits {
			placeholders[i] = "?"
			args = append(args, commits[i])
		}
		query += ` and coalesce(
			json_extract(event, '$.triggerMetadata.push.newSha'),
			json_extract(event, '$.triggerMetadata.pullRequest.sourceSha'),
			json_extract(event, '$.triggerMetadata.manual.sha')
		) in (` + strings.Join(placeholders, ",") + ")"
	}

	if len(kinds) > 0 {
		placeholders := make([]string, len(kinds))
		for i := range kinds {
			placeholders[i] = "?"
			args = append(args, kinds[i])
		}
		query += " and json_extract(event, '$.triggerMetadata.kind') in (" + strings.Join(placeholders, ",") + ")"
	}

	if cursor != "" {
		if cVal, err := strconv.ParseInt(cursor, 10, 64); err == nil {
			query += " and created < ?"
			args = append(args, cVal)
		}
	}

	// First get total count
	var total int64
	countQuery := "select count(*) from (" + query + ")"
	if err := d.QueryRowContext(ctx, countQuery, args...).Scan(&total); err != nil {
		return nil, "", 0, err
	}

	query += " order by created desc limit ?"
	args = append(args, limit)

	rows, err := d.QueryContext(ctx, query, args...)
	if err != nil {
		return nil, "", 0, err
	}
	defer rows.Close()

	var pipelines []*tangled.CiPipeline
	var lastCreated int64

	for rows.Next() {
		var rkey, eventJson string
		var created int64
		if err := rows.Scan(&rkey, &eventJson, &created); err != nil {
			return nil, "", 0, err
		}
		lastCreated = created

		var rawPipeline tangled.Pipeline
		if err := json.Unmarshal([]byte(eventJson), &rawPipeline); err != nil {
			continue
		}

		p, err := d.mapToCiPipeline(rkey, created, rawPipeline)
		if err != nil {
			return nil, "", 0, err
		}
		pipelines = append(pipelines, p)
	}

	nextCursor := ""
	if len(pipelines) == limit {
		nextCursor = strconv.FormatInt(lastCreated, 10)
	}

	return pipelines, nextCursor, total, nil
}

func (d *DB) GetPipeline(ctx context.Context, rkey string) (*tangled.CiPipeline, error) {
	var eventJson string
	var created int64
	err := d.QueryRowContext(ctx,
		`
		select
			event, created from events
		where
			nsid = 'sh.tangled.pipeline'
			and rkey = ?
		`,
		rkey,
	).Scan(&eventJson, &created)

	if err != nil {
		return nil, err
	}

	var rawPipeline tangled.Pipeline
	if err := json.Unmarshal([]byte(eventJson), &rawPipeline); err != nil {
		return nil, err
	}

	return d.mapToCiPipeline(rkey, created, rawPipeline)
}

func (d *DB) mapToCiPipeline(rkey string, created int64, raw tangled.Pipeline) (*tangled.CiPipeline, error) {
	createdAtStr := time.Unix(0, created).Format(time.RFC3339)

	var repoDidStr string
	if raw.TriggerMetadata != nil && raw.TriggerMetadata.Repo != nil {
		if raw.TriggerMetadata.Repo.RepoDid != nil {
			repoDidStr = *raw.TriggerMetadata.Repo.RepoDid
		} else {
			repoDidStr = raw.TriggerMetadata.Repo.Did
		}
	}

	commitSha := ""
	var trigger tangled.CiPipeline_Trigger

	if raw.TriggerMetadata != nil {
		switch workflow.TriggerKind(raw.TriggerMetadata.Kind) {
		case workflow.TriggerKindPush:
			if raw.TriggerMetadata.Push != nil {
				commitSha = raw.TriggerMetadata.Push.NewSha
				trigger.CiTrigger_Push = &tangled.CiTrigger_Push{
					NewSha: raw.TriggerMetadata.Push.NewSha,
					OldSha: raw.TriggerMetadata.Push.OldSha,
					Ref:    raw.TriggerMetadata.Push.Ref,
				}
			}
		case workflow.TriggerKindPullRequest:
			if raw.TriggerMetadata.PullRequest != nil {
				commitSha = raw.TriggerMetadata.PullRequest.SourceSha
				trigger.CiTrigger_PullRequest = &tangled.CiTrigger_PullRequest{
					Action:       raw.TriggerMetadata.PullRequest.Action,
					SourceBranch: &raw.TriggerMetadata.PullRequest.SourceBranch,
					SourceRepo:   raw.TriggerMetadata.SourceRepo,
					SourceSha:    raw.TriggerMetadata.PullRequest.SourceSha,
					TargetBranch: raw.TriggerMetadata.PullRequest.TargetBranch,
					Pull:         raw.TriggerMetadata.PullRequest.Pull,
				}
			}
		case workflow.TriggerKindManual:
			if raw.TriggerMetadata.Manual != nil {
				commitSha = raw.TriggerMetadata.Manual.Sha
				trigger.CiTrigger_Manual = &tangled.CiTrigger_Manual{
					Inputs:     pipelinePairsToCiTriggerPairs(raw.TriggerMetadata.Manual.Inputs),
					Ref:        raw.TriggerMetadata.Manual.Ref,
					Sha:        raw.TriggerMetadata.Manual.Sha,
					SourceRepo: raw.TriggerMetadata.SourceRepo,
				}
			}
		}
	}

	var workflows []*tangled.CiPipeline_Workflow
	for _, wf := range raw.Workflows {
		status := "pending"
		var startedAt, finishedAt, wfError *string

		if raw.TriggerMetadata != nil && raw.TriggerMetadata.Repo != nil {
			wfId := models.WorkflowId{
				PipelineId: models.PipelineId{
					Knot: raw.TriggerMetadata.Repo.Knot,
					Rkey: rkey,
				},
				Name: wf.Name,
			}

			wfStatus, err := d.GetStatus(wfId)
			if err == nil && wfStatus != nil {
				status = wfStatus.Status
				startedAt, finishedAt = d.GetWorkflowTimes(wfId)
				wfError = wfStatus.Error
			}
		}

		workflows = append(workflows, &tangled.CiPipeline_Workflow{
			Id:         wf.Name,
			Name:       wf.Name,
			Status:     status,
			StartedAt:  startedAt,
			FinishedAt: finishedAt,
			Error:      wfError,
		})
	}

	var sourceRepo *string
	if raw.TriggerMetadata != nil {
		sourceRepo = raw.TriggerMetadata.SourceRepo
	}

	return &tangled.CiPipeline{
		Id:         rkey,
		Commit:     commitSha,
		Repo:       &repoDidStr,
		CreatedAt:  &createdAtStr,
		Trigger:    &trigger,
		Workflows:  workflows,
		SourceRepo: sourceRepo,
	}, nil
}

func pipelinePairsToCiTriggerPairs(inputs []*tangled.Pipeline_Pair) []*tangled.CiTrigger_Pair {
	if len(inputs) == 0 {
		return nil
	}
	pairs := make([]*tangled.CiTrigger_Pair, 0, len(inputs))
	for _, input := range inputs {
		if input == nil {
			continue
		}
		pairs = append(pairs, &tangled.CiTrigger_Pair{
			Key:   input.Key,
			Value: input.Value,
		})
	}
	return pairs
}

func (d *DB) GetWorkflowTimes(workflowId models.WorkflowId) (startedAt, finishedAt *string) {
	pipelineAtUri := workflowId.PipelineId.AtUri()

	_ = d.QueryRow(
		`
		select
			min(case when json_extract(event, '$.status') = 'running' then json_extract(event, '$.createdAt') end),
			max(case when json_extract(event, '$.status') in ('success', 'failed', 'timeout', 'cancelled') then json_extract(event, '$.createdAt') end)
		from events
		where
			nsid = ?
			and json_extract(event, '$.pipeline') = ?
			and json_extract(event, '$.workflow') = ?
		`,
		tangled.PipelineStatusNSID,
		string(pipelineAtUri),
		workflowId.Name,
	).Scan(&startedAt, &finishedAt)

	return
}
