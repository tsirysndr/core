package executor

import (
	"context"
	"fmt"
	"sync"

	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
)

// wraps a real engine so StartWorkflows gets the slot ReserveSeat already
// acquired, not a second one. everything else delegates, the execution
// path runs exactly like standalone
type reservedEngine struct {
	models.Engine
	slot engine.WorkflowSlot
	once sync.Once
}

func newReservedEngine(inner models.Engine, slot engine.WorkflowSlot) models.Engine {
	return &reservedEngine{Engine: inner, slot: slot}
}

// hands back the pre-acquired slot exactly once, a second acquire would
// double-count it
func (e *reservedEngine) AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, _ engine.AcquireMode) (engine.WorkflowSlot, error) {
	var slot engine.WorkflowSlot
	e.once.Do(func() {
		slot = e.slot
		e.slot = nil
	})
	if slot == nil {
		return nil, fmt.Errorf("reserved slot already consumed")
	}
	return slot, nil
}
