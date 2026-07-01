package models

import (
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	spindle "tangled.org/core/spindle/models"
	"tangled.org/core/workflow"
)

type Pipeline struct {
	Id        int
	Rkey      string
	Knot      string
	RepoOwner syntax.DID
	RepoName  string
	RepoDid   string
	TriggerId int
	Sha       string
	Created   time.Time

	// populate when querying for reverse mappings
	Trigger  *Trigger
	Statuses map[string]WorkflowStatus
}

func (p *Pipeline) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://did:web:%s/%s/%s", p.Knot, tangled.PipelineNSID, p.Rkey))
}

type WorkflowStatus struct {
	Data []PipelineStatus
}

func (w WorkflowStatus) Latest() PipelineStatus {
	return w.Data[len(w.Data)-1]
}

// time taken by this workflow to reach an "end state"
func (w WorkflowStatus) TimeTaken() time.Duration {
	var start, end *time.Time
	for _, s := range w.Data {
		if s.Status.IsStart() {
			start = &s.Created
		}
		if s.Status.IsFinish() {
			end = &s.Created
		}
	}

	if start != nil && end != nil && end.After(*start) {
		return end.Sub(*start)
	}

	return 0
}

// produces short summary of successes:
// - "0/4" when zero successes of 4 workflows
// - "4/4" when all successes of 4 workflows
// - "0/0" when no workflows run in this pipeline
func (p Pipeline) ShortStatusSummary() string {
	counts := make(map[spindle.StatusKind]int)
	for _, w := range p.Statuses {
		counts[w.Latest().Status] += 1
	}

	total := len(p.Statuses)
	successes := counts[spindle.StatusKindSuccess]

	return fmt.Sprintf("%d/%d", successes, total)
}

// produces a string of the form "3/4 success, 2/4 failed, 1/4 pending"
func (p Pipeline) LongStatusSummary() string {
	counts := make(map[spindle.StatusKind]int)
	for _, w := range p.Statuses {
		counts[w.Latest().Status] += 1
	}

	total := len(p.Statuses)

	var result []string
	// finish states first, followed by start states
	states := append(spindle.FinishStates[:], spindle.StartStates[:]...)
	for _, state := range states {
		if count, ok := counts[state]; ok {
			result = append(result, fmt.Sprintf("%d/%d %s", count, total, state.String()))
		}
	}

	return strings.Join(result, ", ")
}

func (p Pipeline) Counts() map[string]int {
	m := make(map[string]int)
	for _, w := range p.Statuses {
		m[w.Latest().Status.String()] += 1
	}
	return m
}

func (p Pipeline) TimeTaken() time.Duration {
	var s time.Duration
	for _, w := range p.Statuses {
		s += w.TimeTaken()
	}
	return s
}

func (p Pipeline) Workflows() []string {
	var ws []string
	for v := range p.Statuses {
		ws = append(ws, v)
	}
	slices.Sort(ws)
	return ws
}

// if we know that a spindle has picked up this pipeline, then it is Responding
func (p Pipeline) IsResponding() bool {
	return len(p.Statuses) != 0
}

type Trigger struct {
	Id   int
	Kind workflow.TriggerKind

	// push trigger fields
	PushRef    *string
	PushNewSha *string
	PushOldSha *string

	// pull request trigger fields
	PRSourceBranch *string
	PRTargetBranch *string
	PRSourceSha    *string
	PRAction       *string
}

func (t *Trigger) IsPush() bool {
	return t != nil && t.Kind == workflow.TriggerKindPush
}

func (t *Trigger) IsPullRequest() bool {
	return t != nil && t.Kind == workflow.TriggerKindPullRequest
}

func (t *Trigger) TargetRef() string {
	if t.IsPush() {
		return plumbing.ReferenceName(*t.PushRef).Short()
	} else if t.IsPullRequest() {
		return *t.PRTargetBranch
	}

	return ""
}

type PipelineStatus struct {
	ID           int
	Spindle      string
	Rkey         string
	PipelineKnot string
	PipelineRkey string
	Created      time.Time
	Workflow     string
	Status       spindle.StatusKind
	Error        *string
	ExitCode     int
}

func (ps PipelineStatus) ErrorMessage() string {
	if ps.Error == nil {
		return ""
	}
	line, _, _ := strings.Cut(*ps.Error, "\n")
	return line
}

func (ps PipelineStatus) ErrorDetails() string {
	if ps.Error == nil {
		return ""
	}
	_, rest, _ := strings.Cut(*ps.Error, "\n")
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

func (ps *PipelineStatus) PipelineAt() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://did:web:%s/%s/%s", ps.PipelineKnot, tangled.PipelineNSID, ps.PipelineRkey))
}
