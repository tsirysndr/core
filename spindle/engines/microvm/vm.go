//go:build linux

package microvm

import (
	"context"
	"crypto/rand"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"math"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync/atomic"
	"time"

	"tangled.org/core/spindle/models"
)

const (
	minGuestCID         = 3
	vmCrashLogTailBytes = 8192
)

func AllocateCID() (uint32, error) {
	var data [4]byte
	if _, err := rand.Read(data[:]); err != nil {
		return 0, fmt.Errorf("allocate guest CID: %w", err)
	}
	return minGuestCID + binary.BigEndian.Uint32(data[:])%60000, nil
}

func prepareWorkDir(workDir string) error {
	if workDir == "" {
		return fmt.Errorf("microvm work directory is required")
	}
	if err := os.MkdirAll(workDir, 0o755); err != nil {
		return fmt.Errorf("create microvm work directory: %w", err)
	}
	return nil
}

func mkfsExt4ForVolumes(volumes []Volume, configured string) (string, error) {
	if len(volumes) == 0 || configured != "" {
		return configured, nil
	}
	path, err := exec.LookPath("mkfs.ext4")
	if err != nil {
		return "", fmt.Errorf("mkfs.ext4 command not found in PATH: %w", err)
	}
	return path, nil
}

func prepareVolumes(ctx context.Context, workDir string, volumes []Volume, mkfsExt4 string) (map[string]string, error) {
	paths := make(map[string]string, len(volumes))
	for _, volume := range volumes {
		if volume.ReadOnly {
			return nil, fmt.Errorf("read-only microvm volume %q is not supported yet", volume.Image)
		}
		if volume.FSType != "ext4" {
			return nil, fmt.Errorf("microvm volume %q uses unsupported fsType %q", volume.Image, volume.FSType)
		}
		if volume.ImageType != "" && volume.ImageType != "raw" {
			return nil, fmt.Errorf("microvm volume %q uses unsupported imageType %q", volume.Image, volume.ImageType)
		}

		path := filepath.Join(workDir, filepath.Base(volume.Image))
		if err := createSparseFile(path, volume.SizeMiB); err != nil {
			return nil, err
		}
		noJournal := volume.MountPoint == "/workspace"
		if err := runMkfsExt4(ctx, mkfsExt4, path, noJournal); err != nil {
			return nil, err
		}
		paths[volume.Image] = path
	}
	return paths, nil
}

func createSparseFile(path string, sizeMiB int64) error {
	if sizeMiB <= 0 {
		return fmt.Errorf("sparse file %q size must be positive", path)
	}
	if sizeMiB > math.MaxInt64/(1024*1024) {
		return fmt.Errorf("sparse file %q size is too large", path)
	}
	file, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fmt.Errorf("create sparse file %q: %w", path, err)
	}
	defer file.Close()

	if err := file.Truncate(sizeMiB * 1024 * 1024); err != nil {
		return fmt.Errorf("resize sparse file %q: %w", path, err)
	}
	return nil
}

func runMkfsExt4(ctx context.Context, mkfsExt4, path string, noJournal bool) error {
	if mkfsExt4 == "" {
		return fmt.Errorf("mkfs.ext4 path is required")
	}
	args := []string{"-F"}
	if noJournal {
		args = append(args, "-O", "^has_journal")
	}
	args = append(args, path)

	cmd := exec.CommandContext(ctx, mkfsExt4, args...)
	output, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("mkfs.ext4 %q: %w: %s", path, err, strings.TrimSpace(string(output)))
	}
	return nil
}

func createParentedFile(path string) (*os.File, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, fmt.Errorf("create log directory: %w", err)
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
	if err != nil {
		return nil, fmt.Errorf("create log file %q: %w", path, err)
	}
	return file, nil
}

type VMLogs struct {
	Serial string
	Extra  map[string]string
}

type VMHandle interface {
	Shutdown(ctx context.Context) error
	WaitContext(ctx context.Context) error
	Close() error
	Logs() VMLogs
	CID() uint32
	WorkDir() string
	OOMKilled() bool
}

type VMConfig struct {
	Image     ImageSpec
	CID       uint32
	EnableKVM bool
	WorkDir   string
	Cgroup    CgroupLimits

	BootTimeout time.Duration
	MkfsExt4    string
	Dev         bool
}

type workflowState struct {
	ImageSpec              ImageSpec
	ImageSpecPath          string
	Config                 manifestConfig
	ConfigKey              string
	Image                  string
	CacheReadURLs          []string
	CacheTrustedPublicKeys []string
	VM                     VMHandle
	CID                    uint32
	Agent                  *AgentSession
	ReadCache              *ReadCacheProxy
	UploadCache            *UploadCacheProxy
	DNSProxy               *DNSProxy
	WorkDir                string
	NixOSToplevelCache     nixosToplevelCacheStore
	StartedAt              time.Time // when the VM booted, for the max-lifetime cap
}

func (e *Engine) cleanupState(ctx context.Context, wid models.WorkflowId, state *workflowState) error {
	if state == nil {
		return nil
	}

	// stop advertising this VM for debug shells before we tear it down
	e.unregisterDebugTarget(wid)

	ctx = context.WithoutCancel(ctx)

	var err error
	// todo(dawn): expose this error to the user as a warning
	if drainErr := e.drainNixCache(ctx, state); drainErr != nil {
		e.l.Warn("cache drain failed during cleanup; continuing", "workflow", wid, "error", drainErr)
	}
	err = errors.Join(err, e.shutdownVM(ctx, wid, state))
	err = errors.Join(err, closeIO(&state.Agent))
	err = errors.Join(err, closeIO(&state.ReadCache))
	err = errors.Join(err, closeIO(&state.UploadCache))
	err = errors.Join(err, closeIO(&state.DNSProxy))
	err = errors.Join(err, removeWorkDir(state))
	return err
}

func (e *Engine) drainNixCache(ctx context.Context, state *workflowState) error {
	if e.cfg.NixCache.UploadURL == "" {
		return nil
	}

	drainCtx, cancel := context.WithTimeout(ctx, cacheDrainTimeout)
	defer cancel()

	if state.Agent != nil {
		if _, err := state.Agent.Drain(drainCtx); err != nil {
			return fmt.Errorf("drain guest nix cache uploads: %w", err)
		}
	}
	return nil
}

func (e *Engine) shutdownVM(ctx context.Context, wid models.WorkflowId, state *workflowState) error {
	if state.VM == nil {
		return nil
	}
	if vmExited(state.VM) {
		return closeIO(&state.VM)
	}

	var poweroffErr error

	if state.Agent != nil {
		gracefulCtx, cancel := context.WithTimeout(ctx, vmShutdownTimeout)
		var poweredOff bool
		poweredOff, poweroffErr = e.poweroffViaAgent(gracefulCtx, wid, state)
		cancel()

		if poweredOff {
			return closeIO(&state.VM)
		}
		if vmExited(state.VM) {
			return closeIO(&state.VM)
		}
	}

	fallbackCtx, cancel := context.WithTimeout(ctx, vmShutdownTimeout)
	defer cancel()

	shutdownErr := state.VM.Shutdown(fallbackCtx)
	if shutdownErr != nil && !vmExited(state.VM) {
		e.l.Warn("microVM shutdown fallback failed", "workflow", wid, "error", shutdownErr)
		return errors.Join(poweroffErr, shutdownErr, closeIO(&state.VM))
	}

	return closeIO(&state.VM)
}

func vmExited(vm VMHandle) bool {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	// a cancelled wait means the process is still live
	// any other result means it exited
	return !errors.Is(vm.WaitContext(ctx), context.Canceled)
}

func (e *Engine) poweroffViaAgent(ctx context.Context, wid models.WorkflowId, state *workflowState) (bool, error) {
	if err := state.Agent.Poweroff(ctx); err != nil {
		e.l.Warn("agent poweroff request failed", "workflow", wid, "error", err)
		return false, err
	}

	if err := state.VM.WaitContext(ctx); err != nil {
		e.l.Warn("agent poweroff did not stop microVM", "workflow", wid, "error", err)
		return false, nil
	}

	return true, nil
}

// helper for closing io interfaces, sets to nil to prevent double-close
func closeIO[T io.Closer](field *T) error {
	closer := *field
	var zero T
	*field = zero
	if any(closer) == any(zero) {
		return nil
	}
	return closer.Close()
}

func removeWorkDir(state *workflowState) error {
	if state.WorkDir == "" {
		return nil
	}

	err := os.RemoveAll(state.WorkDir)
	state.WorkDir = ""
	return err
}

// returns a context derived from ctx that is cancelled either when ctx itself
// is cancelled or when the microVM exits on its own. the returned flag reports
// whether the VM exited (as opposed to ctx being cancelled for another reason,
// e.g. the workflow timeout), letting callers tell a crash apart from a
// timeout. cancel must be called to release the watcher goroutine.
func watchVMExit(ctx context.Context, vm VMHandle) (context.Context, *atomic.Bool, context.CancelFunc) {
	exited := &atomic.Bool{}
	watchCtx, cancel := context.WithCancel(ctx)
	if vm == nil {
		return watchCtx, exited, cancel
	}
	go func() {
		_ = vm.WaitContext(watchCtx) // returns when VM exits or watchCtx is cancelled
		if watchCtx.Err() == nil {
			exited.Store(true)
			cancel() // don't forget to cancel the watchCtx...
		}
	}()
	return watchCtx, exited, cancel
}

func VMCrashLog(vm VMHandle) string {
	if vm == nil {
		return ""
	}
	logs := vm.Logs()

	var b strings.Builder
	if tail := tailFile(logs.Serial, vmCrashLogTailBytes); tail != "" {
		fmt.Fprintf(&b, "==== serial log ====\n%s\n", tail)
	}
	for _, name := range slices.Sorted(maps.Keys(logs.Extra)) {
		if tail := tailFile(logs.Extra[name], vmCrashLogTailBytes); tail != "" {
			fmt.Fprintf(&b, "==== %s log ====\n%s\n", name, tail)
		}
	}
	return strings.TrimRight(b.String(), "\n")
}

func tailFile(path string, max int64) string {
	if path == "" {
		return ""
	}
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()
	if info, err := f.Stat(); err == nil && info.Size() > max {
		if _, err := f.Seek(-max, io.SeekEnd); err != nil {
			return ""
		}
	}
	data, err := io.ReadAll(f)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(data))
}

func waitAgentConn(ctx context.Context, connCh <-chan net.Conn) (net.Conn, error) {
	select {
	case conn := <-connCh:
		if conn == nil {
			return nil, fmt.Errorf("agent connection closed before setup")
		}
		return conn, nil
	case <-ctx.Done():
		return nil, fmt.Errorf("waiting for agent: %w", ctx.Err())
	}
}

func StartVM(ctx context.Context, cfg VMConfig, logger *slog.Logger) (VMHandle, error) {
	if logger == nil {
		logger = slog.Default()
	}

	runner, err := runnerFor(cfg.Image.RunnerType)
	if err != nil {
		return nil, err
	}
	if err := cfg.Image.Validate(); err != nil {
		return nil, err
	}
	if err := cfg.Image.validateImageFiles(); err != nil {
		return nil, err
	}
	if err := runner.Validate(cfg.Image, cfg.EnableKVM); err != nil {
		return nil, err
	}

	if err := prepareWorkDir(cfg.WorkDir); err != nil {
		return nil, err
	}

	mkfsExt4, err := mkfsExt4ForVolumes(cfg.Image.Volumes, cfg.MkfsExt4)
	if err != nil {
		return nil, err
	}
	volumePaths, err := prepareVolumes(ctx, cfg.WorkDir, cfg.Image.Volumes, mkfsExt4)
	if err != nil {
		return nil, err
	}

	return runner.Start(ctx, cfg, volumePaths, logger)
}

// checks serial log for ooms or kernel panic
// this is very linux specific! but these strings are stable in linux itself, see mm/oom_kill.c and kernel/panic.c
func ParseCrashLog(detail string) (error, bool) {
	if strings.Contains(detail, "Out of memory:") {
		// we can show process name where possible
		re := regexp.MustCompile(`Out of memory: Killed process \d+ \(([^)]+)\)`)
		matches := re.FindStringSubmatch(detail)
		if len(matches) > 1 {
			return fmt.Errorf("guest out of memory (process '%s' killed by guest kernel OOM)", matches[1]), true
		}
		return errors.New("guest out of memory (OOM killer invoked)"), true
	}
	if strings.Contains(detail, "Kernel panic") {
		return errors.New("guest kernel panic"), true
	}
	return nil, false
}
