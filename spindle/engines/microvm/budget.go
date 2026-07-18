//go:build linux

package microvm

import (
	"context"
	"fmt"
	"time"

	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
)

// memory buffer for qemu process / slirp4netns itself
const runnerBufferMiB = 96

type Resources struct {
	MemoryMiB int64
	VCPUs     int64
	DiskMiB   int64
}

func (r Resources) Fits(limit Resources) bool {
	if limit.MemoryMiB > 0 && r.MemoryMiB > limit.MemoryMiB {
		return false
	}
	if limit.VCPUs > 0 && r.VCPUs > limit.VCPUs {
		return false
	}
	if limit.DiskMiB > 0 && r.DiskMiB > limit.DiskMiB {
		return false
	}
	return true
}

func (r Resources) Add(other Resources) Resources {
	return Resources{
		MemoryMiB: r.MemoryMiB + other.MemoryMiB,
		VCPUs:     r.VCPUs + other.VCPUs,
		DiskMiB:   r.DiskMiB + other.DiskMiB,
	}
}

func (r Resources) Sub(other Resources) Resources {
	return Resources{
		MemoryMiB: max(0, r.MemoryMiB-other.MemoryMiB),
		VCPUs:     max(0, r.VCPUs-other.VCPUs),
		DiskMiB:   max(0, r.DiskMiB-other.DiskMiB),
	}
}

func (r Resources) String() string {
	return fmt.Sprintf("memory=%dMiB vcpus=%d disk=%dMiB", r.MemoryMiB, r.VCPUs, r.DiskMiB)
}

func newVMBudgetConfig(cfg config.MicroVMPipelines) (Resources, Resources, time.Duration) {
	budget := Resources{
		MemoryMiB: cfg.MaxTotalMemoryMiB,
		VCPUs:     cfg.MaxTotalVCPUs,
		DiskMiB:   cfg.MaxTotalDiskMiB,
	}
	maxReq := Resources{
		MemoryMiB: cfg.MaxWorkflowMemoryMiB,
		VCPUs:     cfg.MaxWorkflowVCPUs,
		DiskMiB:   cfg.MaxWorkflowDiskMiB,
	}
	return budget, maxReq, cfg.AgingThreshold
}

func (e *Engine) AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, mode engine.AcquireMode) (engine.WorkflowSlot, error) {
	state, ok := wf.Data.(*workflowState)
	if !ok || state == nil {
		return nil, fmt.Errorf("microVM workflow state is not initialized")
	}
	if e.scheduler == nil {
		return engine.NoopSlot{}, nil
	}
	req := resourcesForImage(state.ImageSpec)
	return e.scheduler.Acquire(ctx, req, mode)
}

func resourcesForImage(spec ImageSpec) Resources {
	var diskMiB int64
	for _, volume := range spec.Volumes {
		diskMiB += volume.SizeMiB
	}
	return Resources{
		MemoryMiB: int64(spec.MemoryMiB) + runnerBufferMiB,
		VCPUs:     int64(spec.VCPUs),
		DiskMiB:   diskMiB,
	}
}
