//go:build linux

package microvm

import (
	"fmt"
	"os/exec"
	"runtime"

	"tangled.org/core/spindle/engines/microvm/placement"
	"tangled.org/core/spindle/models"
)

func (e *Engine) ValidateWorkflowPlacement(wf *models.Workflow) error {
	state, ok := wf.Data.(*workflowState)
	if !ok || state == nil {
		return fmt.Errorf("microVM workflow state is not initialized")
	}
	return e.validateImagePlacement(state.ImageSpec)
}

func (e *Engine) validateImagePlacement(spec ImageSpec) error {
	if err := spec.Validate(); err != nil {
		return err
	}
	if !placement.IsNativeArchitecture(spec.Arch, runtime.GOARCH) {
		return fmt.Errorf("microVM image architecture %q is not native to executor architecture %q", spec.Arch, runtime.GOARCH)
	}
	if err := spec.validateImageFiles(); err != nil {
		return err
	}
	runner, err := runnerFor(spec.RunnerType)
	if err != nil {
		return err
	}
	if err := runner.Validate(spec, e.cfg.MicroVMPipelines.EnableKVM); err != nil {
		return err
	}
	if len(spec.Volumes) > 0 {
		if _, err := exec.LookPath("mkfs.ext4"); err != nil {
			return fmt.Errorf("required host command %q not found in PATH: %w", "mkfs.ext4", err)
		}
		for _, volume := range spec.Volumes {
			if volume.ReadOnly {
				return fmt.Errorf("read-only microvm volume %q is not supported yet", volume.Image)
			}
			if volume.FSType != "ext4" {
				return fmt.Errorf("microvm volume %q uses unsupported fsType %q", volume.Image, volume.FSType)
			}
			if volume.ImageType != "" && volume.ImageType != "raw" {
				return fmt.Errorf("microvm volume %q uses unsupported imageType %q", volume.Image, volume.ImageType)
			}
		}
	}

	request := resourcesForImage(spec)
	if !request.Fits(e.budget) || !request.Fits(e.maxWorkflow) {
		return fmt.Errorf("microVM image resources exceed executor limits: request=%v budget=%v max=%v", request, e.budget, e.maxWorkflow)
	}
	return nil
}
