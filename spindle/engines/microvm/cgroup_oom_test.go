package microvm

import (
	"log/slog"
	"os"
	"os/exec"
	"runtime"
	"testing"
	"time"

	cgroups "github.com/containerd/cgroups/v3"
)

const memhogEnv = "SPINDLE_CGROUP_MEMHOG"

func TestMain(m *testing.M) {
	if os.Getenv(memhogEnv) == "1" {
		runMemhogChild()
		return
	}
	os.Exit(m.Run())
}

// this will allocate memory in steps until either the cgroup kills the process
// this is running on, or if the limit is reached. the limit is there so that if
// the cgroup somehow does not work, we don't kill the host and can observe that
// failure.
func runMemhogChild() {
	var b [1]byte
	_, _ = os.Stdin.Read(b[:])

	const chunk = 4 << 20   // 4 MiB
	const limit = 512 << 20 // safety cap
	hold := make([][]byte, 0, limit/chunk)
	for total := 0; total < limit; total += chunk {
		c := make([]byte, chunk)
		for i := range c {
			c[i] = 1 // fault the pages in so they count against memory.current
		}
		hold = append(hold, c)
		time.Sleep(5 * time.Millisecond)
	}
	runtime.KeepAlive(hold)
	os.Exit(0)
}

// creates a cgroup parent, adds a memory limited child to it, and creates a
// process that hogs memory and observes if it OOMs or not.
//
// run with:
//
//	SPINDLE_CGROUP_INTEGRATION=1 systemd-run --user --scope -p Delegate=yes \
//	  go test -run TestCgroupOOMEnforcement ./spindle/engines/microvm/
func TestCgroupOOMEnforcement(t *testing.T) {
	if os.Getenv("SPINDLE_CGROUP_INTEGRATION") != "1" {
		t.Skip("see test doc comment on how to run")
	}
	if cgroups.Mode() != cgroups.Unified {
		t.Skip("requires cgroup v2 unified mode")
	}

	logger := slog.Default()

	parent, err := initCgroupParent(cgroupParentSelf, 0, logger)
	if err != nil {
		t.Skipf("cannot initialize cgroup parent (need cgroup v2 delegation): %v", err)
	}

	swap := int64(0) // disable swap so the limit forces an OOM promptly
	handle, err := prepareCgroup(CgroupLimits{
		Enabled:      true,
		Parent:       parent,
		Name:         "cgtest-oom",
		MemoryMaxMiB: 64,
		SwapMaxMiB:   &swap,
		PidsMax:      256,
	}, logger)
	if err != nil {
		t.Skipf("cannot create a memory-limited child cgroup (need the memory controller delegated): %v", err)
	}
	if handle == nil {
		t.Fatal("prepareCgroup returned a nil handle for enabled limits")
	}
	t.Cleanup(func() { _ = handle.Close() })

	cmd := exec.Command(os.Args[0])
	cmd.Env = append(os.Environ(), memhogEnv+"=1")
	stdin, err := cmd.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	}()

	if err := handle.AddProcess(cmd.Process.Pid, logger); err != nil {
		t.Fatalf("add memhog to cgroup: %v", err)
	}

	// let the child process start allocating memory
	if _, err := stdin.Write([]byte("g")); err != nil {
		t.Fatalf("release memhog: %v", err)
	}
	_ = stdin.Close()

	waitErr := make(chan error, 1)
	go func() { waitErr <- cmd.Wait() }()

	select {
	case err := <-waitErr:
		if err == nil {
			t.Fatal("memhog exited cleanly: the cgroup memory limit was not enforced")
		}
		t.Logf("memhog died as expected: %v", err)
	case <-time.After(30 * time.Second):
		t.Fatal("memhog did not die within 30s, cgroup memory limit not enforced")
	}

	if !handle.OOMKilled() {
		t.Fatal("OOMKilled() is false after the memhog was killed, memory.events oom_kill was not observed")
	}
}
