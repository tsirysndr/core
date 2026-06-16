//go:build linux && integration

package sandbox

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/landlock-lsm/go-landlock/landlock"
)

// This file exercises actual Landlock enforcement. It works by re-execing the
// test binary with an env var that makes it run the "child" code path: the
// child applies the ruleset built by BuildRuleSpec and attempts a specific
// filesystem operation, exiting 0 (allowed) or non-zero (denied).
//
// Build tag: linux && integration. Run with:
//   go test -tags integration ./knotserver/sandbox/...

const (
	childEnv    = "TANGLED_SANDBOX_INT_CHILD"
	childRepo   = "TANGLED_SANDBOX_INT_REPO"
	childTarget = "TANGLED_SANDBOX_INT_TARGET"
	childOp     = "TANGLED_SANDBOX_INT_OP"
)

func TestMain(m *testing.M) {
	if os.Getenv(childEnv) == "1" {
		runChild()
		return
	}
	os.Exit(m.Run())
}

func runChild() {
	repoPath := os.Getenv(childRepo)
	target := os.Getenv(childTarget)
	op := os.Getenv(childOp)

	spec := BuildRuleSpec([]string{repoPath})
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
	if err := landlock.V8.BestEffort().RestrictPaths(rules...); err != nil {
		fmt.Fprintf(os.Stderr, "restrict failed: %v\n", err)
		os.Exit(2)
	}

	var opErr error
	switch op {
	case "read":
		_, opErr = os.ReadFile(target)
	case "write":
		opErr = os.WriteFile(target, []byte("x"), 0644)
	case "list":
		_, opErr = os.ReadDir(target)
	default:
		fmt.Fprintf(os.Stderr, "unknown op %q\n", op)
		os.Exit(3)
	}

	if opErr != nil {
		fmt.Fprintln(os.Stderr, opErr)
		os.Exit(1)
	}
	os.Exit(0)
}

// runUnderSandbox spawns the test binary as a child, applies Landlock with
// the given repoPath as the granted RW path, then attempts op on target.
// Returns true if the op was allowed, false if denied.
func runUnderSandbox(t *testing.T, repoPath, target, op string) (allowed bool, output string) {
	t.Helper()
	cmd := exec.Command(os.Args[0])
	cmd.Env = append(os.Environ(),
		childEnv+"=1",
		childRepo+"="+repoPath,
		childTarget+"="+target,
		childOp+"="+op,
	)
	out, err := cmd.CombinedOutput()
	if err == nil {
		return true, string(out)
	}
	if exit, ok := err.(*exec.ExitError); ok && exit.ExitCode() == 1 {
		return false, string(out)
	}
	t.Fatalf("child exited unexpectedly: %v\noutput: %s", err, out)
	return false, ""
}

func skipIfNoLandlock(t *testing.T) {
	t.Helper()
	if !probeLandlock() {
		t.Skip("Landlock not available on this kernel")
	}
}

func TestSandboxIntegration_AllowsOwnRepoRead(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	repo := filepath.Join(root, "did:plc:abc")
	target := filepath.Join(repo, "HEAD")
	mustMkdirAll(t, repo)
	mustWriteFile(t, target, "ref: refs/heads/main\n")

	allowed, out := runUnderSandbox(t, repo, target, "read")
	if !allowed {
		t.Errorf("expected read of own repo to be allowed; child said: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_DeniesOtherRepoRead(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	myRepo := filepath.Join(root, "did:plc:abc")
	otherRepo := filepath.Join(root, "did:plc:xyz")
	secret := filepath.Join(otherRepo, "secret-key")
	mustMkdirAll(t, myRepo)
	mustMkdirAll(t, otherRepo)
	mustWriteFile(t, secret, "TOPSECRET\n")

	allowed, out := runUnderSandbox(t, myRepo, secret, "read")
	if allowed {
		t.Errorf("expected read of other repo to be denied; child read: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_AllowsGlobalConfigRead(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	myRepo := filepath.Join(root, "did:plc:abc")
	cfgDir := filepath.Join(root, ".config", "git")
	cfg := filepath.Join(cfgDir, "config")
	mustMkdirAll(t, myRepo)
	mustMkdirAll(t, cfgDir)
	mustWriteFile(t, cfg, "[user]\n\tname = test\n")

	// the child reads $HOME via BuildRuleSpec, so set HOME for it explicitly.
	t.Setenv("HOME", root)

	allowed, out := runUnderSandbox(t, myRepo, cfg, "read")
	if !allowed {
		t.Errorf("expected read of $HOME/.config/git/config to be allowed; child said: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_DeniesGlobalConfigSibling(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	myRepo := filepath.Join(root, "did:plc:abc")
	cfgDir := filepath.Join(root, ".config", "git")
	sibling := filepath.Join(cfgDir, "attributes")
	mustMkdirAll(t, myRepo)
	mustMkdirAll(t, cfgDir)
	mustWriteFile(t, sibling, "*.bin -text\n")

	t.Setenv("HOME", root)

	allowed, out := runUnderSandbox(t, myRepo, sibling, "read")
	if allowed {
		t.Errorf("expected read of sibling in .config/git/ to be denied (only the config file is granted); child read: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_DeniesScanPathSibling(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	myRepo := filepath.Join(root, "did:plc:abc")
	dbFile := filepath.Join(root, "knotserver.db")
	mustMkdirAll(t, myRepo)
	mustWriteFile(t, dbFile, "fake db contents\n")

	allowed, out := runUnderSandbox(t, myRepo, dbFile, "read")
	if allowed {
		t.Errorf("expected read of sibling file (knotserver.db) to be denied; child read: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_DeniesScanPathListing(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	myRepo := filepath.Join(root, "did:plc:abc")
	mustMkdirAll(t, myRepo)

	allowed, out := runUnderSandbox(t, myRepo, root, "list")
	if allowed {
		t.Errorf("expected listing scan path to be denied; child output: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_AllowsOwnRepoWrite(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	repo := filepath.Join(root, "did:plc:abc")
	target := filepath.Join(repo, "new-file")
	mustMkdirAll(t, repo)

	allowed, out := runUnderSandbox(t, repo, target, "write")
	if !allowed {
		t.Errorf("expected write to own repo to be allowed; child said: %s", strings.TrimSpace(out))
	}
}

func TestSandboxIntegration_DeniesSystemWrite(t *testing.T) {
	skipIfNoLandlock(t)

	root := t.TempDir()
	repo := filepath.Join(root, "did:plc:abc")
	mustMkdirAll(t, repo)

	// /etc is granted RO; writes must be denied.
	allowed, out := runUnderSandbox(t, repo, "/etc/sandbox-test-should-fail", "write")
	if allowed {
		t.Errorf("expected write to /etc to be denied; child output: %s", strings.TrimSpace(out))
	}
}

func mustMkdirAll(t *testing.T, p string) {
	t.Helper()
	if err := os.MkdirAll(p, 0o755); err != nil {
		t.Fatal(err)
	}
}

func mustWriteFile(t *testing.T, p, content string) {
	t.Helper()
	if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}
