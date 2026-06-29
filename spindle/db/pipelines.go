package db

import (
	"context"
	"encoding/json"
	"strconv"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
)

func (d *DB) QueryPipelines(ctx context.Context, repoDid string, commits []string, cursor string, limit int) ([]*tangled.CiDefs_Pipeline, string, int64, error) {
	if limit <= 0 {
		limit = 30
	}

	var query string
	var args []interface{}
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
		query += " and json_extract(event, '$.triggerMetadata.push.newSha') in (" + strings.Join(placeholders, ",") + ")"
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

	var pipelines []*tangled.CiDefs_Pipeline
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

		p, err := d.mapToCiDefsPipeline(ctx, rkey, created, rawPipeline)
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

func (d *DB) GetPipeline(ctx context.Context, rkey string) (*tangled.CiDefs_Pipeline, error) {
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

	return d.mapToCiDefsPipeline(ctx, rkey, created, rawPipeline)
}

func (d *DB) mapToCiDefsPipeline(ctx context.Context, rkey string, created int64, raw tangled.Pipeline) (*tangled.CiDefs_Pipeline, error) {
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
	var trigger tangled.CiDefs_Pipeline_Trigger

	if raw.TriggerMetadata != nil {
		switch raw.TriggerMetadata.Kind {
		case "push":
			if raw.TriggerMetadata.Push != nil {
				commitSha = raw.TriggerMetadata.Push.NewSha
				trigger.CiTrigger_Push = &tangled.CiTrigger_Push{
					NewSha: raw.TriggerMetadata.Push.NewSha,
					OldSha: raw.TriggerMetadata.Push.OldSha,
					Ref:    raw.TriggerMetadata.Push.Ref,
				}
			}
		case "pullRequest":
			if raw.TriggerMetadata.PullRequest != nil {
				commitSha = raw.TriggerMetadata.PullRequest.SourceSha
				trigger.CiTrigger_PullRequest = &tangled.CiTrigger_PullRequest{
					Action:       raw.TriggerMetadata.PullRequest.Action,
					SourceBranch: &raw.TriggerMetadata.PullRequest.SourceBranch,
					SourceSha:    raw.TriggerMetadata.PullRequest.SourceSha,
					TargetBranch: raw.TriggerMetadata.PullRequest.TargetBranch,
				}
			}
		case "manual":
			if raw.TriggerMetadata.Manual != nil {
				trigger.CiTrigger_Manual = &tangled.CiTrigger_Manual{}
			}
		}
	}

	var workflows []*tangled.CiDefs_Workflow
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

		workflows = append(workflows, &tangled.CiDefs_Workflow{
			Id:         wf.Name,
			Name:       wf.Name,
			Status:     status,
			StartedAt:  startedAt,
			FinishedAt: finishedAt,
			Error:      wfError,
		})
	}

	return &tangled.CiDefs_Pipeline{
		Id:        rkey,
		Commit:    commitSha,
		Repo:      &repoDidStr,
		CreatedAt: &createdAtStr,
		Trigger:   &trigger,
		Workflows: workflows,
	}, nil
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
