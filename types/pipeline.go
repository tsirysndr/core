package types

import (
	"fmt"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
)

type StatusKind string

func (s StatusKind) String() string {
	return string(s)
}

func (s StatusKind) IsFinish() bool {
	switch string(s) {
	case "failed", "timeout", "cancelled", "success":
		return true
	}
	return false
}

func (s StatusKind) IsStart() bool {
	switch string(s) {
	case "pending", "running":
		return true
	}
	return false
}

type WorkflowStatus struct {
	*tangled.CiDefs_Workflow
	PipelineCreatedAt *string
}

func (w WorkflowStatus) Latest() WorkflowStatus {
	return w
}

func (w WorkflowStatus) Error() string {
	if w.CiDefs_Workflow == nil || w.CiDefs_Workflow.Error == nil {
		return ""
	}
	return *w.CiDefs_Workflow.Error
}

func (w WorkflowStatus) Status() StatusKind {
	if w.CiDefs_Workflow == nil {
		return ""
	}
	return StatusKind(w.CiDefs_Workflow.Status)
}

func (w WorkflowStatus) TimeTaken() time.Duration {
	if w.CiDefs_Workflow == nil || w.StartedAt == nil || w.FinishedAt == nil || *w.StartedAt == "" || *w.FinishedAt == "" {
		return 0
	}
	t1, err1 := time.Parse(time.RFC3339, *w.StartedAt)
	t2, err2 := time.Parse(time.RFC3339, *w.FinishedAt)
	if err1 == nil && err2 == nil && t2.After(t1) {
		return t2.Sub(t1)
	}
	return 0
}

func (w WorkflowStatus) Created() time.Time {
	var timeStr string
	if w.CiDefs_Workflow != nil && w.StartedAt != nil && *w.StartedAt != "" {
		timeStr = *w.StartedAt
	} else if w.CiDefs_Workflow != nil && w.FinishedAt != nil && *w.FinishedAt != "" {
		timeStr = *w.FinishedAt
	} else if w.PipelineCreatedAt != nil && *w.PipelineCreatedAt != "" {
		timeStr = *w.PipelineCreatedAt
	} else {
		return time.Time{}
	}

	t, err := time.Parse(time.RFC3339, timeStr)
	if err != nil {
		return time.Time{}
	}
	return t
}

type Trigger struct {
	*tangled.CiDefs_Pipeline_Trigger
}

func (t Trigger) IsPush() bool {
	return t.CiDefs_Pipeline_Trigger != nil && t.CiDefs_Pipeline_Trigger.CiTrigger_Push != nil
}

func (t Trigger) IsPullRequest() bool {
	return t.CiDefs_Pipeline_Trigger != nil && t.CiDefs_Pipeline_Trigger.CiTrigger_PullRequest != nil
}

func (t Trigger) TargetRef() string {
	if t.CiDefs_Pipeline_Trigger == nil {
		return ""
	}
	if t.CiDefs_Pipeline_Trigger.CiTrigger_Push != nil {
		ref := t.CiDefs_Pipeline_Trigger.CiTrigger_Push.Ref
		if strings.HasPrefix(ref, "refs/heads/") {
			return strings.TrimPrefix(ref, "refs/heads/")
		}
		if strings.HasPrefix(ref, "refs/tags/") {
			return strings.TrimPrefix(ref, "refs/tags/")
		}
		return ref
	}
	if t.CiDefs_Pipeline_Trigger.CiTrigger_PullRequest != nil {
		return t.CiDefs_Pipeline_Trigger.CiTrigger_PullRequest.TargetBranch
	}
	return ""
}

func (t Trigger) PRSourceBranch() string {
	if t.CiDefs_Pipeline_Trigger == nil || t.CiDefs_Pipeline_Trigger.CiTrigger_PullRequest == nil {
		return ""
	}
	sb := t.CiDefs_Pipeline_Trigger.CiTrigger_PullRequest.SourceBranch
	if sb == nil {
		return ""
	}
	return *sb
}

type Pipeline struct {
	*tangled.CiDefs_Pipeline
}

func (p Pipeline) Valid() bool {
	return p.CiDefs_Pipeline != nil
}

func (p Pipeline) Id() string {
	if p.CiDefs_Pipeline == nil {
		return ""
	}
	return p.CiDefs_Pipeline.Id
}

func (p Pipeline) Statuses() map[string]WorkflowStatus {
	m := make(map[string]WorkflowStatus)
	if p.CiDefs_Pipeline != nil {
		for _, w := range p.CiDefs_Pipeline.Workflows {
			m[w.Name] = WorkflowStatus{
				CiDefs_Workflow:   w,
				PipelineCreatedAt: p.CreatedAt,
			}
		}
	}
	return m
}

func (p Pipeline) Counts() map[string]int {
	m := make(map[string]int)
	if p.CiDefs_Pipeline != nil {
		for _, w := range p.CiDefs_Pipeline.Workflows {
			m[w.Status]++
		}
	}
	return m
}

func (p Pipeline) ShortStatusSummary() string {
	if p.CiDefs_Pipeline == nil {
		return ""
	}
	counts := p.Counts()
	total := len(p.CiDefs_Pipeline.Workflows)
	successes := counts["success"]
	return fmt.Sprintf("%d/%d", successes, total)
}

func (p Pipeline) LongStatusSummary() string {
	if p.CiDefs_Pipeline == nil {
		return ""
	}
	counts := p.Counts()
	total := len(p.CiDefs_Pipeline.Workflows)
	var parts []string
	states := []string{"success", "failed", "timeout", "cancelled", "running", "pending"}
	for _, state := range states {
		if c, ok := counts[state]; ok {
			parts = append(parts, fmt.Sprintf("%d/%d %s", c, total, state))
		}
	}
	return strings.Join(parts, ", ")
}

func (p Pipeline) TimeTaken() time.Duration {
	if p.CiDefs_Pipeline == nil {
		return 0
	}
	var s time.Duration
	for _, w := range p.CiDefs_Pipeline.Workflows {
		s += WorkflowStatus{CiDefs_Workflow: w}.TimeTaken()
	}
	return s
}

func (p Pipeline) Created() time.Time {
	if p.CiDefs_Pipeline == nil || p.CreatedAt == nil || *p.CreatedAt == "" {
		return time.Time{}
	}
	t, err := time.Parse(time.RFC3339, *p.CreatedAt)
	if err != nil {
		return time.Time{}
	}
	return t
}

func (p Pipeline) Trigger() Trigger {
	if p.CiDefs_Pipeline == nil {
		return Trigger{nil}
	}
	return Trigger{p.CiDefs_Pipeline.Trigger}
}

func (p Pipeline) IsResponding() bool {
	return p.CiDefs_Pipeline != nil && len(p.CiDefs_Pipeline.Workflows) > 0
}

func (p Pipeline) Sha() string {
	if p.CiDefs_Pipeline == nil {
		return ""
	}
	return p.CiDefs_Pipeline.Commit
}

func (p Pipeline) Workflows() []string {
	var ws []string
	if p.CiDefs_Pipeline != nil {
		for _, w := range p.CiDefs_Pipeline.Workflows {
			ws = append(ws, w.Name)
		}
	}
	return ws
}
