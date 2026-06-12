package microvm

import (
	"testing"
)

func TestSanitizeCgroupName(t *testing.T) {
	cases := []struct {
		in   string
		want string
	}{
		{"workflow-abc123", "workflow-abc123"},
		{"a/b:c", "a-b-c"},
		{"--lead--", "lead"},
		{"a__b..c", "a-b-c"},
		{"keep.dots_and-dashes", "keep.dots_and-dashes"},
		{"", ""},
		{"///", ""},
	}
	for _, tc := range cases {
		if got := sanitizeCgroupName(tc.in); got != tc.want {
			t.Errorf("sanitizeCgroupName(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}

func TestCgroupResourcesSwapOnlyStillSetsMemory(t *testing.T) {
	swap := int64(8)
	r := cgroupResources(CgroupLimits{SwapMaxMiB: &swap})
	if r.Memory == nil {
		t.Fatal("a swap limit alone should still produce a memory controller config")
	}
	if r.Memory.Max != nil {
		t.Errorf("memory max should be unset when only swap is limited, got %v", *r.Memory.Max)
	}
	if r.Memory.Swap == nil || *r.Memory.Swap != 8*1024*1024 {
		t.Errorf("swap = %v, want %d bytes", r.Memory.Swap, 8*1024*1024)
	}
}
