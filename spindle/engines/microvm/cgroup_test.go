package microvm

import (
	"log/slog"
	"os"
	"testing"

	cgroups "github.com/containerd/cgroups/v3"
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

// regression test for the cgroup-namespace-root case: at a populated
// namespace root the engine must vacate the parent before enabling
// subtree controllers.
//
// run with:
//
//	go test -c -o microvm.test ./spindle/engines/microvm/
//	podman run --rm --cap-add SYS_ADMIN --security-opt seccomp=unconfined \
//	  -v $PWD/microvm.test:/t:Z -e SPINDLE_CGROUP_INTEGRATION=1 \
//	  --entrypoint /bin/sh docker.io/library/golang:1.25 \
//	  -c "mount -t cgroup2 cgroup2 /sys/fs/cgroup && exec /t -test.run TestCgroupParentVacatesPopulatedNamespaceRoot"
func TestCgroupParentVacatesPopulatedNamespaceRoot(t *testing.T) {
	if os.Getenv("SPINDLE_CGROUP_INTEGRATION") != "1" {
		t.Skip("see test doc comment on how to run")
	}
	if cgroups.Mode() != cgroups.Unified {
		t.Skip("requires cgroup v2 unified mode")
	}

	group, err := selfCgroupV2Path()
	if err != nil {
		t.Fatal(err)
	}
	if group != "/" {
		t.Skipf("only meaningful at a cgroup namespace root, self cgroup is %q", group)
	}

	logger := slog.Default()
	parent, err := initCgroupParent(cgroupParentSelf, 0, logger)
	if err != nil {
		t.Fatalf("initCgroupParent at a cgroup namespace root: %v", err)
	}

	procs, err := parent.root.Procs(false)
	if err != nil {
		t.Fatalf("list parent cgroup processes: %v", err)
	}
	if len(procs) != 0 {
		t.Errorf("namespace root still holds %d processes after initCgroupParent; "+
			"enabling subtree controllers for microVM cgroups would fail EBUSY", len(procs))
	}

	handle, err := prepareCgroup(CgroupLimits{
		Enabled:      true,
		Parent:       parent,
		Name:         "cgtest-nsroot",
		MemoryMaxMiB: 64,
		PidsMax:      256,
	}, logger)
	if err != nil {
		t.Fatalf("create controller-enabled child at namespace root: %v", err)
	}
	t.Cleanup(func() { _ = handle.Close() })
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
