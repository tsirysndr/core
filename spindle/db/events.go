package db

import (
	"encoding/json"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/eventstream"
	"tangled.org/core/notifier"
	"tangled.org/core/spindle/models"
	"tangled.org/core/tid"
)

func (d *DB) insertEvent(event eventstream.Event, n *notifier.Notifier) error {
	return eventstream.Insert(d, event, n)
}

func (d *DB) GetEvents(cursor int64, limit int) ([]eventstream.Event, error) {
	return eventstream.List(d, cursor, limit)
}

func (d *DB) CreatePipelineEvent(rkey string, pipeline tangled.Pipeline, n *notifier.Notifier) error {
	eventJson, err := json.Marshal(pipeline)
	if err != nil {
		return err
	}
	event := eventstream.Event{
		Rkey:      rkey,
		Nsid:      tangled.PipelineNSID,
		EventJson: eventJson,
	}
	return d.insertEvent(event, n)
}

func (d *DB) createStatusEvent(
	workflowId models.WorkflowId,
	statusKind models.StatusKind,
	workflowError *string,
	exitCode *int64,
	n *notifier.Notifier,
) error {
	now := time.Now()
	pipelineAtUri := workflowId.PipelineId.AtUri()
	s := tangled.PipelineStatus{
		CreatedAt: now.Format(time.RFC3339),
		Error:     workflowError,
		ExitCode:  exitCode,
		Pipeline:  string(pipelineAtUri),
		Workflow:  workflowId.Name,
		Status:    string(statusKind),
	}

	eventJson, err := json.Marshal(s)
	if err != nil {
		return err
	}

	event := eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      tangled.PipelineStatusNSID,
		EventJson: eventJson,
	}

	return d.insertEvent(event, n)
}

func (d *DB) GetStatus(workflowId models.WorkflowId) (*tangled.PipelineStatus, error) {
	pipelineAtUri := workflowId.PipelineId.AtUri()

	var eventJson string
	err := d.QueryRow(
		`
		select
			event from events
		where
			nsid = ?
			and json_extract(event, '$.pipeline') = ?
			and json_extract(event, '$.workflow') = ?
		order by
			created desc
		limit
			1
		`,
		tangled.PipelineStatusNSID,
		string(pipelineAtUri),
		workflowId.Name,
	).Scan(&eventJson)

	if err != nil {
		return nil, err
	}

	var status tangled.PipelineStatus
	if err := json.Unmarshal([]byte(eventJson), &status); err != nil {
		return nil, err
	}

	return &status, nil
}

func (d *DB) StatusPending(workflowId models.WorkflowId, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindPending, nil, nil, n)
}

func (d *DB) StatusRunning(workflowId models.WorkflowId, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindRunning, nil, nil, n)
}

func (d *DB) StatusFailed(workflowId models.WorkflowId, workflowError string, exitCode int64, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindFailed, &workflowError, &exitCode, n)
}

func (d *DB) StatusCancelled(workflowId models.WorkflowId, workflowError string, exitCode int64, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindCancelled, &workflowError, &exitCode, n)
}

func (d *DB) StatusSuccess(workflowId models.WorkflowId, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindSuccess, nil, nil, n)
}

func (d *DB) StatusTimeout(workflowId models.WorkflowId, n *notifier.Notifier) error {
	return d.createStatusEvent(workflowId, models.StatusKindTimeout, nil, nil, n)
}
