//go:build linux

package sandbox

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"unsafe"

	"github.com/landlock-lsm/go-landlock/landlock"
	"golang.org/x/sys/unix"
)

var ErrUnsupportedPlatform = errors.New("no sandbox backend available")

// LandlockBackend uses the Linux Landlock LSM via a re-exec pattern.
// landlock_restrict_self only affects the calling OS thread, so we re-exec
// the binary as "sandbox-exec" which runs single-threaded before exec'ing git.
type LandlockBackend struct {
	selfExe string
	lookup  LookupUID
}

func (l *LandlockBackend) Wrap(repoPath string, cmd *exec.Cmd) (*exec.Cmd, error) {
	return l.WrapMulti([]string{repoPath}, cmd)
}

func (l *LandlockBackend) WrapMulti(paths []string, cmd *exec.Cmd) (*exec.Cmd, error) {
	if len(paths) == 0 {
		return cmd, nil
	}

	// resolve the executable to an absolute path now, while $PATH is still
	// intact; the re-exec'd sandbox-exec subprocess inherits the env we pass
	// via cmd.Env, which may not include the wrappers that set up $PATH.
	args := cmd.Args
	if len(args) > 0 {
		if abs, err := exec.LookPath(args[0]); err == nil {
			args = append([]string{abs}, args[1:]...)
		}
	}

	var sandboxArgs []string
	sandboxArgs = append(sandboxArgs, "sandbox-exec")
	for _, p := range paths {
		sandboxArgs = append(sandboxArgs, "--repo-path="+p)
	}
	sandboxArgs = append(sandboxArgs, "--")
	sandboxArgs = append(sandboxArgs, args...)

	wrapped := exec.Command(l.selfExe, sandboxArgs...)
	wrapped.Env = cmd.Env
	wrapped.Dir = paths[0] // kernel chdir's here after setuid, before execve
	wrapped.Stdin = cmd.Stdin
	wrapped.Stdout = cmd.Stdout
	wrapped.Stderr = cmd.Stderr

	// drop to the virtual UID if we can resolve one. the kernel handles
	// fork -> setresuid -> chdir -> execve; requires CAP_SETUID/GID on the caller.
	//
	// the primary GID is intentionally set to the virtual UID, NOT the
	// repo's group ownership. repo dirs are owned by virtualUID:gitGroup
	// with mode 0770 so the knot service (in gitGroup) can read them, but
	// sandbox subprocesses must not inherit gitGroup or they would gain
	// group access to every other repo and lose cross-owner isolation.
	if l.lookup != nil {
		if uid, _, err := l.lookup(paths[0]); err == nil && uid > 0 {
			wrapped.SysProcAttr = &syscall.SysProcAttr{
				Credential: &syscall.Credential{Uid: uid, Gid: uid, NoSetGroups: true},
			}
		}
	}

	return wrapped, nil
}

func (l *LandlockBackend) Name() string { return "landlock" }

// ApplyLandlock applies a Landlock ruleset to the current process then
// exec's into gitArgs. Called from the hidden "sandbox-exec" subcommand.
func ApplyLandlock(repoPaths []string, gitArgs []string) error {
	if len(gitArgs) == 0 {
		return fmt.Errorf("sandbox-exec: no command specified")
	}

	// collect unique parent directories so git can read global config
	// under $HOME/.config/git/config. repo contents stay DAC-locked
	// (0700) so other repos can't actually be read.
	parents := map[string]struct{}{}
	for _, p := range repoPaths {
		parents[filepath.Dir(p)] = struct{}{}
	}
	parentSlice := make([]string, 0, len(parents))
	for p := range parents {
		parentSlice = append(parentSlice, p)
	}

	// each repo gets full read/write plus REFER (needed for git's quarantine
	// rename in receive-pack, which moves objects across directories).
	repoRules := make([]landlock.Rule, len(repoPaths))
	for i, p := range repoPaths {
		repoRules[i] = landlock.RWDirs(p).WithRefer()
	}

	rules := append([]landlock.Rule{
		// system dirs: read + execute only, no writes
		landlock.RODirs("/usr", "/bin", "/lib", "/lib64", "/nix", "/etc").IgnoreIfMissing(),
		// /dev/null and friends: read/write files + ioctl (V5+ restricts ioctl
		// on device files; WithIoctlDev keeps /dev/null fully accessible)
		landlock.RWFiles("/dev").WithIoctlDev().IgnoreIfMissing(),
		// parent dirs: read + execute so git can traverse to the repo and read
		// global git config; 0700 DAC permissions prevent cross-repo reads
		landlock.RODirs(parentSlice...).IgnoreIfMissing(),
		// /tmp: read/write for temporary patch and object files
		landlock.RWDirs("/tmp").IgnoreIfMissing(),
	}, repoRules...)

	// V8.BestEffort enforces the strongest ruleset the running kernel supports,
	// up to V8. RestrictPaths also sets PR_SET_NO_NEW_PRIVS automatically.
	if err := landlock.V8.BestEffort().RestrictPaths(rules...); err != nil {
		return fmt.Errorf("sandbox-exec: restrict paths: %w", err)
	}

	gitBin := gitArgs[0]
	if !filepath.IsAbs(gitBin) {
		return fmt.Errorf("sandbox-exec: expected absolute path, got %q", gitBin)
	}

	return unix.Exec(gitBin, gitArgs, os.Environ())
}

func probeLandlock() bool {
	_, err := landlockCreateRuleset(nil, unix.LANDLOCK_CREATE_RULESET_VERSION)
	// EOPNOTSUPP and ENOSYS mean the kernel doesn't support landlock.
	// Any other result (including EINVAL for the nil attr) means it's available.
	return !errors.Is(err, unix.EOPNOTSUPP) && !errors.Is(err, unix.ENOSYS)
}

func platformNew(lookup LookupUID) (Backend, string) {
	if probeLandlock() {
		selfExe, err := os.Readlink("/proc/self/exe")
		if err != nil {
			selfExe = "/proc/self/exe"
		}
		return &LandlockBackend{selfExe: selfExe, lookup: lookup}, ""
	}

	return &NoopBackend{}, "landlock unavailable (kernel < 5.13); git subprocesses run unsandboxed"
}

func platformProbe() string {
	if probeLandlock() {
		return "landlock available (kernel >= 5.13)"
	}
	return "no sandbox backend available (kernel < 5.13)"
}

// landlockCreateRuleset wraps the landlock_create_ruleset(2) syscall.
// Pass attr=nil and flags=LANDLOCK_CREATE_RULESET_VERSION to query ABI version.
// Used only for the non-destructive probe in probeLandlock; all ruleset
// construction is handled by go-landlock.
func landlockCreateRuleset(attr *unix.LandlockRulesetAttr, flags uint) (int, error) {
	var attrPtr unsafe.Pointer
	var attrSize uintptr
	if attr != nil {
		attrPtr = unsafe.Pointer(attr)
		attrSize = unsafe.Sizeof(*attr)
	}
	fd, _, errno := unix.Syscall(
		unix.SYS_LANDLOCK_CREATE_RULESET,
		uintptr(attrPtr),
		attrSize,
		uintptr(flags),
	)
	if errno != 0 {
		return 0, errno
	}
	return int(fd), nil
}
