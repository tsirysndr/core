package db

import (
	"context"
	"database/sql"
	"encoding/json"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
)

type JobRow struct {
	Id             int64
	RepoDid        string
	PipelineIdKnot string
	PipelineIdRkey string
	SourceRepo     *tangled.Pipeline_TriggerRepo
	Tpl            tangled.Pipeline
}

func (d *DB) EnqueueJob(ctx context.Context, repoDid string, pipelineId models.PipelineId, sourceRepo *tangled.Pipeline_TriggerRepo, tpl tangled.Pipeline) error {
	tplJson, err := json.Marshal(tpl)
	if err != nil {
		return err
	}
	_, err = d.ExecContext(ctx, `
		insert into jobs (repo_did, pipeline_id_knot, pipeline_id_rkey, source_repo, tpl)
		values (?, ?, ?, ?, ?)
		`, repoDid, pipelineId.Knot, pipelineId.Rkey, string(sourceRepoJson(sourceRepo)), string(tplJson))
	return err
}
func (d *DB) DequeueJob(ctx context.Context) (*JobRow, error) {
	var row JobRow
	var sourceRepoStr *string
	var tplJson string
	err := d.QueryRowContext(ctx, `
		delete from jobs
		where id = (
			select id from jobs
			order by id asc
			limit 1
		)
		returning id, repo_did, pipeline_id_knot, pipeline_id_rkey, source_repo, tpl
	`).Scan(&row.Id, &row.RepoDid, &row.PipelineIdKnot, &row.PipelineIdRkey, &sourceRepoStr, &tplJson)
	if err != nil {
		if err == sql.ErrNoRows {
			return nil, nil
		}
		return nil, err
	}
	if err := json.Unmarshal([]byte(tplJson), &row.Tpl); err != nil {
		return nil, err
	}
	if sourceRepoStr != nil {
		row.SourceRepo = &tangled.Pipeline_TriggerRepo{}
		if err := json.Unmarshal([]byte(*sourceRepoStr), row.SourceRepo); err != nil {
			return nil, err
		}
	}
	return &row, nil
}

func sourceRepoJson(sr *tangled.Pipeline_TriggerRepo) []byte {
	if sr == nil {
		return nil
	}
	b, _ := json.Marshal(sr)
	return b
}
