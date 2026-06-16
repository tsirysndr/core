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

// RuleSpec describes the paths a sandbox should grant access to, grouped by
// access tier. It is the input to the Landlock ruleset construction and is
// exposed so the path-derivation logic can be tested independently of any
// actual kernel-level enforcement.
type RuleSpec struct {
	// SystemRO is the set of system directories granted read+execute.
	SystemRO []string
	// GitConfigRO is the global git config file, granted read-only access
	// at file granularity. Empty when $HOME is not set.
	GitConfigRO string
	// DevRW is the set of device-file directories granted read/write +
	// ioctl access (needed so /dev/null works under Landlock V5+).
	DevRW []string
	// TmpRW is the set of directories granted read/write for temporary
	// patch and object files.
	TmpRW []string
	// RepoRW is the set of repository directories granted read/write
	// access including the REFER right (for cross-directory rename in
	// receive-pack's quarantine migration).
	RepoRW []string
}

// BuildRuleSpec derives the set of paths the sandbox should grant to each
// access tier given the repository paths the subprocess operates on.
func BuildRuleSpec(repoPaths []string) RuleSpec {
	return buildRuleSpec(repoPaths, os.Getenv("HOME"))
}

// buildRuleSpec is the testable variant of BuildRuleSpec that takes $HOME
// explicitly instead of reading it from the environment.
func buildRuleSpec(repoPaths []string, home string) RuleSpec {
	var gitConfig string
	if home != "" {
		// the only thing the sandboxed git subprocess needs from $HOME is the
		// global config file. granting just that one file (not the whole
		// .config tree) keeps everything else under $HOME outside the ruleset.
		gitConfig = filepath.Join(home, ".config", "git", "config")
	}

	return RuleSpec{
		SystemRO:    []string{"/usr", "/bin", "/lib", "/lib64", "/nix", "/etc"},
		GitConfigRO: gitConfig,
		DevRW:       []string{"/dev"},
		TmpRW:       []string{"/tmp"},
		RepoRW:      append([]string(nil), repoPaths...),
	}
}

// ApplyLandlock applies a Landlock ruleset to the current process then
// exec's into gitArgs. Called from the hidden "sandbox-exec" subcommand.
func ApplyLandlock(repoPaths []string, gitArgs []string) error {
	if len(gitArgs) == 0 {
		return fmt.Errorf("sandbox-exec: no command specified")
	}

	spec := BuildRuleSpec(repoPaths)

	rules := []landlock.Rule{
		landlock.RODirs(spec.SystemRO...).IgnoreIfMissing(),
		landlock.RWFiles(spec.DevRW...).WithIoctlDev().IgnoreIfMissing(),
		landlock.RWDirs(spec.TmpRW...).IgnoreIfMissing(),
	}
	if spec.GitConfigRO != "" {
		rules = append(rules, landlock.ROFiles(spec.GitConfigRO).IgnoreIfMissing())
	}
	for _, p := range spec.RepoRW {
		rules = append(rules, landlock.RWDirs(p).WithRefer())
	}

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
