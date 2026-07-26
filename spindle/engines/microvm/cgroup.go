package microvm

import (
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"syscall"

	cgroups "github.com/containerd/cgroups/v3"
	"github.com/containerd/cgroups/v3/cgroup2"
	"github.com/prometheus/procfs"
)

var (
	cgroupInvalidChar    = regexp.MustCompile(`[^a-zA-Z0-9\-_.]`)
	cgroupConsecutiveSep = regexp.MustCompile(`[-_.]{2,}`)
)

const (
	cgroupParentSelf     = "self"
	supervisorCgroupName = "supervisor"
)

type CgroupLimits struct {
	Enabled      bool
	Parent       *CgroupParent
	Name         string
	MemoryMaxMiB int64
	SwapMaxMiB   *int64
	PidsMax      int64
	// cpu.max quota as a percentage of one core (100 = one core). <= 0
	// leaves cpu unlimited
	CPUQuotaPercent int64
	// io.weight (1-10000). 0 leaves io unlimited, written directly since
	// this cgroup2 lib only models io.bfq.weight/io.max
	IOWeight uint64
}

type CgroupParent struct {
	root       *cgroup2.Manager
	mountpoint string
	group      string
}

type CgroupHandle struct {
	manager *cgroup2.Manager
}

func initCgroupParent(parent string, supervisorMemoryMinMiB int64, logger *slog.Logger) (*CgroupParent, error) {
	if parent == "" {
		parent = cgroupParentSelf
	}
	if cgroups.Mode() != cgroups.Unified {
		return nil, fmt.Errorf("microVM cgroups require cgroup v2 unified mode")
	}

	mountpoint, group, err := resolveCgroupParent(parent)
	if err != nil {
		return nil, err
	}
	if _, err := os.Stat(filepath.Join(mountpoint, strings.TrimPrefix(group, "/"))); err != nil {
		return nil, fmt.Errorf("stat cgroup parent %q:%q: %w", mountpoint, group, err)
	}

	root, err := cgroup2.Load(group, cgroup2.WithMountpoint(mountpoint))
	if err != nil {
		return nil, fmt.Errorf("load cgroup parent %q:%q: %w", mountpoint, group, err)
	}

	if group != "/" {
		if err := moveParentProcesses(root, supervisorMemoryMinMiB, logger); err != nil {
			return nil, err
		}
	} else if err := probeRootSubtreeControl(mountpoint); err != nil {
		if !errors.Is(err, syscall.EBUSY) {
			return nil, fmt.Errorf("enable controllers in subtree_control of cgroup root %q: %w", mountpoint, err)
		}
		// populated namespace root, not the real root: vacate it too
		if err := moveParentProcesses(root, supervisorMemoryMinMiB, logger); err != nil {
			return nil, err
		}
	}

	if logger != nil {
		logger.Info("initialized microVM cgroup parent", "mountpoint", mountpoint, "group", group)
	}
	return &CgroupParent{root: root, mountpoint: mountpoint, group: group}, nil
}

func prepareCgroup(limits CgroupLimits, logger *slog.Logger) (*CgroupHandle, error) {
	if !limits.Enabled {
		return nil, nil
	}
	if limits.Parent == nil || limits.Parent.root == nil {
		return nil, fmt.Errorf("cgroup parent is not initialized")
	}
	name := sanitizeCgroupName(limits.Name)
	if name == "" {
		return nil, fmt.Errorf("cgroup name is empty")
	}

	manager, err := limits.Parent.root.NewChild(name, cgroupResources(limits))
	if err != nil {
		return nil, fmt.Errorf("create cgroup %q: %w", name, err)
	}

	if limits.IOWeight > 0 {
		if err := manager.ToggleControllers([]string{"io"}, cgroup2.Enable); err != nil {
			_ = manager.Delete()
			return nil, fmt.Errorf("enable io controller for cgroup %q: %w", name, err)
		}
		ioWeightPath := filepath.Join(limits.Parent.mountpoint, strings.TrimPrefix(limits.Parent.group, "/"), name, "io.weight")
		if err := os.WriteFile(ioWeightPath, []byte(strconv.FormatUint(limits.IOWeight, 10)), 0); err != nil {
			_ = manager.Delete()
			return nil, fmt.Errorf("write io.weight for cgroup %q: %w", name, err)
		}
	}

	if logger != nil {
		logger.Info("created microVM cgroup", "name", name, "parentGroup", limits.Parent.group)
	}
	return &CgroupHandle{manager: manager}, nil
}

func cgroupResources(limits CgroupLimits) *cgroup2.Resources {
	resources := &cgroup2.Resources{}
	if limits.MemoryMaxMiB > 0 || limits.SwapMaxMiB != nil {
		memory := &cgroup2.Memory{}
		if limits.MemoryMaxMiB > 0 {
			maxBytes := limits.MemoryMaxMiB * 1024 * 1024
			memory.Max = &maxBytes
		}
		if limits.SwapMaxMiB != nil {
			swapBytes := *limits.SwapMaxMiB * 1024 * 1024
			memory.Swap = &swapBytes
		}
		oomGroup := true
		memory.OOMGroup = &oomGroup
		resources.Memory = memory
	}
	if limits.PidsMax > 0 {
		resources.Pids = &cgroup2.Pids{Max: limits.PidsMax}
	}
	if limits.CPUQuotaPercent > 0 {
		quota := limits.CPUQuotaPercent * 1000 // 100% of one 100000us period
		resources.CPU = &cgroup2.CPU{Max: cgroup2.NewCPUMax(&quota, nil)}
	}
	return resources
}

func supervisorResources(memoryMinMiB int64) *cgroup2.Resources {
	if memoryMinMiB <= 0 {
		return nil
	}
	minBytes := memoryMinMiB * 1024 * 1024
	return &cgroup2.Resources{
		Memory: &cgroup2.Memory{Min: &minBytes},
	}
}

func (h *CgroupHandle) AddProcess(pid int, logger *slog.Logger) error {
	if h == nil || h.manager == nil {
		return nil
	}
	if pid <= 0 {
		return fmt.Errorf("invalid pid %d", pid)
	}
	if err := h.manager.AddProc(uint64(pid)); err != nil {
		return fmt.Errorf("add pid %d to cgroup: %w", pid, err)
	}
	if logger != nil {
		logger.Info("added process to microVM cgroup", "pid", pid)
	}
	return nil
}

func (h *CgroupHandle) Close() error {
	if h == nil || h.manager == nil {
		return nil
	}
	return h.manager.Delete()
}

func (h *CgroupHandle) OOMKilled() bool {
	if h == nil || h.manager == nil {
		return false
	}
	metrics, err := h.manager.Stat()
	if err != nil || metrics == nil || metrics.MemoryEvents == nil {
		return false
	}
	return metrics.MemoryEvents.OomKill > 0
}

// probeRootSubtreeControl enables the domain controllers the engine needs
// in the "/" parent's subtree. this fails EBUSY at a populated cgroup
// namespace root (no-internal-process constraint); the real root is exempt.
func probeRootSubtreeControl(mountpoint string) error {
	// cpu/io may be unavailable (eg. no CONFIG_BLK_CGROUP), memory/pids
	// are the hard requirement
	if err := os.WriteFile(filepath.Join(mountpoint, "cgroup.subtree_control"), []byte("+memory +pids +cpu +io"), 0); err == nil {
		return nil
	}
	return os.WriteFile(filepath.Join(mountpoint, "cgroup.subtree_control"), []byte("+memory +pids"), 0)
}

func resolveCgroupParent(parent string) (string, string, error) {
	mountpoint, err := cgroup2Mountpoint()
	if err != nil {
		return "", "", err
	}

	if parent == "" || parent == cgroupParentSelf {
		group, err := selfCgroupV2Path()
		if err != nil {
			return "", "", err
		}
		return mountpoint, group, nil
	}
	if !filepath.IsAbs(parent) {
		return "", "", fmt.Errorf("cgroup parent must be %q or an absolute delegated cgroupfs path: %q", cgroupParentSelf, parent)
	}

	cleanParent := filepath.Clean(parent)
	rel, err := filepath.Rel(mountpoint, cleanParent)
	if err != nil {
		return "", "", fmt.Errorf("resolve cgroup parent %q relative to cgroup2 mount %q: %w", cleanParent, mountpoint, err)
	}
	if rel == ".." || strings.HasPrefix(rel, "../") {
		return "", "", fmt.Errorf("cgroup parent %q is outside cgroup2 mount %q", cleanParent, mountpoint)
	}
	if rel == "." {
		return mountpoint, "/", nil
	}

	group := "/" + filepath.ToSlash(rel)
	if err := cgroup2.VerifyGroupPath(group); err != nil {
		return "", "", fmt.Errorf("invalid cgroup parent path %q: %w", group, err)
	}
	return mountpoint, group, nil
}

func cgroup2Mountpoint() (string, error) {
	mounts, err := procfs.GetMounts()
	if err != nil {
		return "", fmt.Errorf("read procfs mountinfo: %w", err)
	}
	for _, mount := range mounts {
		if mount.FSType == "cgroup2" {
			return mount.MountPoint, nil
		}
	}
	return "", fmt.Errorf("cgroup v2 mountpoint not found")
}

func selfCgroupV2Path() (string, error) {
	self, err := procfs.Self()
	if err != nil {
		return "", fmt.Errorf("open procfs self: %w", err)
	}
	groups, err := self.Cgroups()
	if err != nil {
		return "", fmt.Errorf("read procfs self cgroups: %w", err)
	}
	for _, group := range groups {
		if group.HierarchyID != 0 {
			continue
		}
		path := group.Path
		if path == "" {
			path = "/"
		}
		if err := cgroup2.VerifyGroupPath(path); err != nil {
			return "", fmt.Errorf("invalid self cgroup path %q: %w", path, err)
		}
		return path, nil
	}
	return "", fmt.Errorf("current process has no cgroup v2 hierarchy entry")
}

func moveParentProcesses(parent *cgroup2.Manager, supervisorMemoryMinMiB int64, logger *slog.Logger) error {
	procs, err := parent.Procs(false)
	if err != nil {
		return fmt.Errorf("list parent cgroup processes: %w", err)
	}

	// first create with empty resources
	supervisor, err := parent.NewChild(supervisorCgroupName, &cgroup2.Resources{})
	if err != nil {
		return fmt.Errorf("create supervisor cgroup: %w", err)
	}

	// move procs
	for _, pid := range procs {
		if err := supervisor.AddProc(pid); err != nil {
			return fmt.Errorf("move pid %d to supervisor cgroup: %w", pid, err)
		}
	}

	// now apply resources. we can't do this while parent has procs still
	if res := supervisorResources(supervisorMemoryMinMiB); res != nil {
		// we use a "new" parent here, this is so we enable subtree_control.
		// .Update() does not work here...
		if _, err = parent.NewChild(supervisorCgroupName, res); err != nil {
			return fmt.Errorf("apply supervisor cgroup resources: %w", err)
		}
	}

	if logger != nil && len(procs) > 0 {
		logger.Info("moved spindle processes to supervisor cgroup", "processes", len(procs))
	}
	return nil
}

func sanitizeCgroupName(name string) string {
	name = cgroupInvalidChar.ReplaceAllLiteralString(name, "-")
	name = cgroupConsecutiveSep.ReplaceAllLiteralString(name, "-")
	return strings.Trim(name, "-_.")
}
