package microvm

import (
	"context"
	"fmt"
	"log/slog"
)

type Runner interface {
	// check the host has what this backend needs for spec.
	Validate(spec ImageSpec, enableKVM bool) error
	Start(ctx context.Context, cfg VMConfig, volumePaths map[string]string, logger *slog.Logger) (VMHandle, error)
}

func runnerFor(runnerType string) (Runner, error) {
	switch runnerType {
	case "qemu", "":
		return qemuRunner{}, nil
	case "firecracker":
		return nil, fmt.Errorf("runner type %q not implemented yet", runnerType)
	default:
		return nil, fmt.Errorf("unsupported runner type %q", runnerType)
	}
}
