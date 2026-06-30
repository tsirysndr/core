package ssh

import (
	"time"

	"tangled.org/core/api/tangled"
)

// helper functions against generated code

func workflowElapsed(wf *tangled.CiPipeline_Workflow, now time.Time) time.Duration {
	if wf.StartedAt == nil {
		return 0
	}
	started, err := time.Parse(time.RFC3339, *wf.StartedAt)
	if err != nil {
		return 0
	}
	if wf.FinishedAt == nil {
		return now.Sub(started)
	}
	finished, err := time.Parse(time.RFC3339, *wf.FinishedAt)
	if err != nil {
		return 0
	}
	return finished.Sub(started)
}

var finishedStatuses = map[string]bool{
	"failed":    true,
	"timeout":   true,
	"cancelled": true,
	"success":   true,
}

func pipelineFinished(p *tangled.CiPipeline) bool {
	for _, wf := range p.Workflows {
		if !finishedStatuses[wf.Status] {
			return false
		}
	}
	return true
}
