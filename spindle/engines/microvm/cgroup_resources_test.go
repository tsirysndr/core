//go:build linux

package microvm

import "testing"

func TestCgroupResourcesCPUQuota(t *testing.T) {
	r := cgroupResources(CgroupLimits{CPUQuotaPercent: 200})
	if r.CPU == nil {
		t.Fatal("cpu quota should produce a cpu controller config")
	}
	if got := string(r.CPU.Max); got != "200000 100000" {
		t.Errorf("cpu.max = %q, want %q", got, "200000 100000")
	}

	r = cgroupResources(CgroupLimits{})
	if r.CPU != nil {
		t.Errorf("no quota should leave cpu unlimited, got %v", r.CPU)
	}
}
