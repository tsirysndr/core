package engine

import (
	"context"
	"errors"

	"tangled.org/core/spindle/models"
)

var ErrNoWorkflowSlots = errors.New("no workflow slots available")

type WorkflowSlot interface {
	Release()
}

type WorkflowSlotter interface {
	AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow) (WorkflowSlot, error)
}

type releaseFunc func()

func (f releaseFunc) Release() {
	if f != nil {
		f()
	}
}

type NoopSlot struct{}

func (NoopSlot) Release() {}

// limit by concurrent workflow count
type SemaphoreSlotter struct {
	slots chan struct{}
}

func NewSemaphoreSlotter(maxConcurrent int) *SemaphoreSlotter {
	if maxConcurrent <= 0 {
		return &SemaphoreSlotter{}
	}
	return &SemaphoreSlotter{slots: make(chan struct{}, maxConcurrent)}
}

func (a *SemaphoreSlotter) AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow) (WorkflowSlot, error) {
	if a == nil || a.slots == nil {
		return NoopSlot{}, nil
	}
	select {
	case a.slots <- struct{}{}:
		return releaseFunc(func() { <-a.slots }), nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}
