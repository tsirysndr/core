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
	*tangled.CiPipeline_Workflow
	PipelineCreatedAt *string
}

func (w WorkflowStatus) Latest() WorkflowStatus {
	return w
}

func (w WorkflowStatus) Error() string {
	if w.CiPipeline_Workflow == nil || w.CiPipeline_Workflow.Error == nil {
		return ""
	}
	return *w.CiPipeline_Workflow.Error
}

func (w WorkflowStatus) ErrorMessage() string {
	line, _, _ := strings.Cut(w.Error(), "\n")
	return line
}

func (w WorkflowStatus) ErrorDetails() string {
	_, rest, _ := strings.Cut(w.Error(), "\n")
	if rest == "" {
		return ""
	}

	const truncateTo = 15
	lines := strings.Split(rest, "\n")
	if len(lines) <= truncateTo {
		return rest
	}
	return strings.Join(lines[:truncateTo], "\n") + "\n…"
}

func (w WorkflowStatus) Status() StatusKind {
	if w.CiPipeline_Workflow == nil {
		return ""
	}
	return StatusKind(w.CiPipeline_Workflow.Status)
}

func (w WorkflowStatus) TimeTaken() time.Duration {
	if w.CiPipeline_Workflow == nil || w.StartedAt == nil || w.FinishedAt == nil || *w.StartedAt == "" || *w.FinishedAt == "" {
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
	if w.CiPipeline_Workflow != nil && w.StartedAt != nil && *w.StartedAt != "" {
		timeStr = *w.StartedAt
	} else if w.CiPipeline_Workflow != nil && w.FinishedAt != nil && *w.FinishedAt != "" {
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
	*tangled.CiPipeline_Trigger
}

func (t Trigger) IsPush() bool {
	return t.CiPipeline_Trigger != nil && t.CiPipeline_Trigger.CiTrigger_Push != nil
}

func (t Trigger) IsPullRequest() bool {
	return t.CiPipeline_Trigger != nil && t.CiPipeline_Trigger.CiTrigger_PullRequest != nil
}

func (t Trigger) IsManual() bool {
	return t.CiPipeline_Trigger != nil && t.CiPipeline_Trigger.CiTrigger_Manual != nil
}

func (t Trigger) TargetRef() string {
	if t.CiPipeline_Trigger == nil {
		return ""
	}
	if t.CiPipeline_Trigger.CiTrigger_Push != nil {
		ref := t.CiPipeline_Trigger.CiTrigger_Push.Ref
		if after, ok := strings.CutPrefix(ref, "refs/heads/"); ok {
			return after
		}
		if after, ok := strings.CutPrefix(ref, "refs/tags/"); ok {
			return after
		}
		return ref
	}
	if t.CiPipeline_Trigger.CiTrigger_PullRequest != nil {
		return t.CiPipeline_Trigger.CiTrigger_PullRequest.TargetBranch
	}
	return ""
}

func (t Trigger) PRSourceBranch() string {
	if t.CiPipeline_Trigger == nil || t.CiPipeline_Trigger.CiTrigger_PullRequest == nil {
		return ""
	}
	sb := t.CiPipeline_Trigger.CiTrigger_PullRequest.SourceBranch
	if sb == nil {
		return ""
	}
	return *sb
}

func (t Trigger) PRUri() string {
	if t.CiPipeline_Trigger == nil || t.CiPipeline_Trigger.CiTrigger_PullRequest == nil {
		return ""
	}
	pull := t.CiPipeline_Trigger.CiTrigger_PullRequest.Pull
	if pull == nil {
		return ""
	}
	return *pull
}

type Pipeline struct {
	*tangled.CiPipeline
}

func (p Pipeline) Valid() bool {
	return p.CiPipeline != nil
}

func (p Pipeline) Id() string {
	if p.CiPipeline == nil {
		return ""
	}
	return p.CiPipeline.Id
}

func (p Pipeline) Statuses() map[string]WorkflowStatus {
	m := make(map[string]WorkflowStatus)
	if p.CiPipeline != nil {
		for _, w := range p.CiPipeline.Workflows {
			m[w.Name] = WorkflowStatus{
				CiPipeline_Workflow: w,
				PipelineCreatedAt:   p.CreatedAt,
			}
		}
	}
	return m
}

// PipelinesByCommit keeps the first run per commit from newest-first input.
func PipelinesByCommit(pipelines []*tangled.CiPipeline) map[string]Pipeline {
	m := make(map[string]Pipeline, len(pipelines))
	for _, pipeline := range pipelines {
		if pipeline == nil {
			continue
		}
		if _, ok := m[pipeline.Commit]; ok {
			continue
		}
		m[pipeline.Commit] = Pipeline{CiPipeline: pipeline}
	}
	return m
}

func (p Pipeline) Counts() map[string]int {
	m := make(map[string]int)
	if p.CiPipeline != nil {
		for _, w := range p.CiPipeline.Workflows {
			m[w.Status]++
		}
	}
	return m
}

// InProgress reports whether any workflow in the pipeline is still pending or
// running (i.e. the pipeline has not fully settled).
func (p Pipeline) InProgress() bool {
	counts := p.Counts()
	return counts["pending"] > 0 || counts["running"] > 0
}

func (p Pipeline) ShortStatusSummary() string {
	if p.CiPipeline == nil {
		return ""
	}
	counts := p.Counts()
	total := len(p.CiPipeline.Workflows)
	successes := counts["success"]
	return fmt.Sprintf("%d/%d", successes, total)
}

func (p Pipeline) LongStatusSummary() string {
	if p.CiPipeline == nil {
		return ""
	}
	counts := p.Counts()
	total := len(p.CiPipeline.Workflows)
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
	if p.CiPipeline == nil {
		return 0
	}
	var s time.Duration
	for _, w := range p.CiPipeline.Workflows {
		s += WorkflowStatus{CiPipeline_Workflow: w}.TimeTaken()
	}
	return s
}

func (p Pipeline) Created() time.Time {
	if p.CiPipeline == nil || p.CreatedAt == nil || *p.CreatedAt == "" {
		return time.Time{}
	}
	t, err := time.Parse(time.RFC3339, *p.CreatedAt)
	if err != nil {
		return time.Time{}
	}
	return t
}

func (p Pipeline) Trigger() Trigger {
	if p.CiPipeline == nil {
		return Trigger{nil}
	}
	return Trigger{p.CiPipeline.Trigger}
}

func (p Pipeline) IsResponding() bool {
	return p.CiPipeline != nil && len(p.CiPipeline.Workflows) > 0
}

func (p Pipeline) Sha() string {
	if p.CiPipeline == nil {
		return ""
	}
	return p.CiPipeline.Commit
}

// where the pipeline commit was checked out from, nil when checked out from repo itself
func (p Pipeline) SourceRepo() *string {
	if p.CiPipeline == nil {
		return nil
	}
	return p.CiPipeline.SourceRepo
}

func (p Pipeline) Workflows() []string {
	var ws []string
	if p.CiPipeline != nil {
		for _, w := range p.CiPipeline.Workflows {
			ws = append(ws, w.Name)
		}
	}
	return ws
}
