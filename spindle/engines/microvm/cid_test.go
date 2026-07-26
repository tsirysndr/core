package microvm

import (
	"testing"
)

func TestBuildConfigKeyScopesByRepo(t *testing.T) {
	spec := ImageSpec{BaseConfigHash: "deadbeef"}
	cfg := manifestConfig{Dependencies: []string{"nodejs"}}

	a, err := buildConfigKey(spec, cfg, "did:plc:aaa")
	if err != nil {
		t.Fatal(err)
	}
	b, err := buildConfigKey(spec, cfg, "did:plc:bbb")
	if err != nil {
		t.Fatal(err)
	}
	if a == b {
		t.Errorf("same config in different repos produced the same key %q", a)
	}

	again, err := buildConfigKey(spec, cfg, "did:plc:aaa")
	if err != nil {
		t.Fatal(err)
	}
	if a != again {
		t.Errorf("key not deterministic: %q vs %q", a, again)
	}
}

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
