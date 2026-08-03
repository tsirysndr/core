//go:build !linux

package microvm

import (
	"fmt"
	"log/slog"
)

type CgroupLimits struct {
	Enabled         bool
	Parent          *CgroupParent
	Name            string
	MemoryMaxMiB    int64
	SwapMaxMiB      *int64
	PidsMax         int64
	CPUQuotaPercent int64
	IOWeight        uint64
}

type CgroupParent struct{}

type CgroupHandle struct{}

func initCgroupParent(string, int64, *slog.Logger) (*CgroupParent, error) {
	return nil, fmt.Errorf("microVM cgroups are only supported on Linux")
}

func prepareCgroup(limits CgroupLimits, _ *slog.Logger) (*CgroupHandle, error) {
	if !limits.Enabled {
		return nil, nil
	}
	return nil, fmt.Errorf("microVM cgroups are only supported on Linux")
}

func (*CgroupHandle) AddProcess(int, *slog.Logger) error {
	return nil
}

func (*CgroupHandle) Close() error {
	return nil
}

func (*CgroupHandle) OOMKilled() bool {
	return false
}
