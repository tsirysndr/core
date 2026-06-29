//go:build linux

package sandbox

import (
	"os/exec"
	"reflect"
	"strings"
	"syscall"
	"testing"
)

func TestBuildRuleSpec_SingleRepo(t *testing.T) {
	spec := buildRuleSpec([]string{"/home/git/did:plc:abc"}, "/home/git")

	if got, want := spec.GitConfigRO, "/home/git/.config/git/config"; got != want {
		t.Errorf("GitConfigRO = %q, want %q", got, want)
	}
	if got, want := spec.RepoRW, []string{"/home/git/did:plc:abc"}; !reflect.DeepEqual(got, want) {
		t.Errorf("RepoRW = %q, want %q", got, want)
	}
	if got, want := spec.SystemRO, []string{"/usr", "/bin", "/lib", "/lib64", "/nix", "/etc"}; !reflect.DeepEqual(got, want) {
		t.Errorf("SystemRO = %q, want %q", got, want)
	}
	if got, want := spec.TmpRW, []string{"/tmp"}; !reflect.DeepEqual(got, want) {
		t.Errorf("TmpRW = %q, want %q", got, want)
	}
	if got, want := spec.DevRW, []string{"/dev"}; !reflect.DeepEqual(got, want) {
		t.Errorf("DevRW = %q, want %q", got, want)
	}
}

func TestBuildRuleSpec_GitConfigFollowsHome(t *testing.T) {
	// the granted git config path must follow $HOME, not the repo path. this
	// is what makes the merge case work: tmpDir is under /tmp, but the
	// subprocess still resolves the global config from $HOME/.config/git/config.
	spec := buildRuleSpec([]string{"/tmp/git-clone-XYZ"}, "/home/git")

	if got, want := spec.GitConfigRO, "/home/git/.config/git/config"; got != want {
		t.Errorf("GitConfigRO = %q, want %q", got, want)
	}
}

func TestBuildRuleSpec_NoHome(t *testing.T) {
	// empty $HOME should not produce a bogus "/.config/git/config" entry.
	spec := buildRuleSpec([]string{"/home/git/did:plc:abc"}, "")

	if spec.GitConfigRO != "" {
		t.Errorf("GitConfigRO = %q, want empty", spec.GitConfigRO)
	}
}

func TestBuildRuleSpec_NeverGrantsScanPath(t *testing.T) {
	// the scan path (the repo's parent) must NEVER appear in any RW or RO
	// list. granting it would expose other repos and the knot DB via
	// Landlock RO + DAC group bits. this is the key invariant the rule
	// tightening was meant to enforce.
	spec := buildRuleSpec([]string{"/home/git/did:plc:abc"}, "/home/git")

	parent := "/home/git"
	for _, group := range [][]string{spec.SystemRO, spec.DevRW, spec.TmpRW, spec.RepoRW} {
		for _, p := range group {
			if p == parent {
				t.Errorf("scan path %q must not appear in the ruleset; found in %q", parent, group)
			}
		}
	}
	if spec.GitConfigRO == parent {
		t.Errorf("scan path %q must not be granted as GitConfigRO", parent)
	}
}

func TestBuildRuleSpec_EmptyInput(t *testing.T) {
	spec := buildRuleSpec(nil, "")
	if spec.GitConfigRO != "" {
		t.Errorf("GitConfigRO should be empty for nil input and no $HOME, got %q", spec.GitConfigRO)
	}
	if len(spec.RepoRW) != 0 {
		t.Errorf("RepoRW should be empty for nil input, got %q", spec.RepoRW)
	}
}

func TestLandlockBackend_Name(t *testing.T) {
	if (&LandlockBackend{}).Name() != "landlock" {
		t.Error("Name should return \"landlock\"")
	}
}

func TestLandlockBackend_WrapMulti_NoPaths(t *testing.T) {
	sb := &LandlockBackend{selfExe: "/proc/self/exe"}
	cmd := exec.Command("git", "status")

	wrapped, err := sb.WrapMulti(nil, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped != cmd {
		t.Error("empty paths should return the original cmd unchanged")
	}
}

func TestLandlockBackend_WrapMulti_ArgsConstruction(t *testing.T) {
	sb := &LandlockBackend{selfExe: "/path/to/knot"}
	cmd := exec.Command("git", "upload-pack", "--stateless-rpc", ".")
	cmd.Env = []string{"GIT_PROTOCOL=version=2"}
	cmd.Stdin = strings.NewReader("input")

	wrapped, err := sb.WrapMulti([]string{"/repos/a", "/repos/b"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}

	// argv[0] is the selfExe
	if wrapped.Path != "/path/to/knot" {
		t.Errorf("Path = %q, want %q", wrapped.Path, "/path/to/knot")
	}

	// argv should be: [knot, sandbox-exec, --repo-path=/repos/a, --repo-path=/repos/b, --, <abs-git>, upload-pack, ...]
	args := wrapped.Args
	if len(args) < 6 {
		t.Fatalf("Args too short: %v", args)
	}
	if args[0] != "/path/to/knot" {
		t.Errorf("Args[0] = %q, want %q", args[0], "/path/to/knot")
	}
	if args[1] != "sandbox-exec" {
		t.Errorf("Args[1] = %q, want %q", args[1], "sandbox-exec")
	}
	if args[2] != "--repo-path=/repos/a" {
		t.Errorf("Args[2] = %q, want --repo-path=/repos/a", args[2])
	}
	if args[3] != "--repo-path=/repos/b" {
		t.Errorf("Args[3] = %q, want --repo-path=/repos/b", args[3])
	}
	if args[4] != "--" {
		t.Errorf("Args[4] = %q, want --", args[4])
	}
	// args[5] is the resolved absolute git path; just check it ends with /git
	if !strings.HasSuffix(args[5], "/git") && args[5] != "git" {
		t.Errorf("Args[5] = %q, want path ending in /git or bare \"git\"", args[5])
	}
	if got := args[len(args)-1]; got != "." {
		t.Errorf("last arg = %q, want %q", got, ".")
	}

	// Dir should be the first repo path so the kernel chdirs there
	// after setuid, before execve.
	if wrapped.Dir != "/repos/a" {
		t.Errorf("Dir = %q, want %q", wrapped.Dir, "/repos/a")
	}

	// Env propagated
	if len(wrapped.Env) != 1 || wrapped.Env[0] != "GIT_PROTOCOL=version=2" {
		t.Errorf("Env = %v, want [GIT_PROTOCOL=version=2]", wrapped.Env)
	}

	// Stdio propagated
	if wrapped.Stdin != cmd.Stdin {
		t.Error("Stdin not propagated to wrapped cmd")
	}
}

func TestLandlockBackend_WrapMulti_NoLookupNoCredential(t *testing.T) {
	sb := &LandlockBackend{selfExe: "/path/to/knot"} // no lookup
	cmd := exec.Command("git", "status")

	wrapped, err := sb.WrapMulti([]string{"/repos/a"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.SysProcAttr != nil && wrapped.SysProcAttr.Credential != nil {
		t.Error("no lookup configured; Credential should not be set")
	}
}

func TestLandlockBackend_WrapMulti_LookupSetsCredential(t *testing.T) {
	// lookup deliberately returns a different gid (the service group) to
	// confirm that WrapMulti ignores it and uses uid as the primary gid.
	// see the comment in sandbox_linux.go for why this matters.
	sb := &LandlockBackend{
		selfExe: "/path/to/knot",
		lookup: func(repoPath string) (uint32, uint32, error) {
			if repoPath != "/repos/a" {
				t.Errorf("lookup called with %q, want /repos/a", repoPath)
			}
			return 100042, 1234, nil
		},
	}
	cmd := exec.Command("git", "status")

	wrapped, err := sb.WrapMulti([]string{"/repos/a"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.SysProcAttr == nil || wrapped.SysProcAttr.Credential == nil {
		t.Fatal("Credential should be set when lookup returns uid > 0")
	}
	cred := wrapped.SysProcAttr.Credential
	if cred.Uid != 100042 {
		t.Errorf("Credential.Uid = %d, want 100042", cred.Uid)
	}
	if cred.Gid != 100042 {
		t.Errorf("Credential.Gid = %d, want 100042 (must equal Uid, not lookup's gid 1234)", cred.Gid)
	}
	// NoSetGroups must be false (the default) so the kernel calls
	// setgroups(0, NULL) and clears supplementary groups. NoSetGroups: true
	// would let the subprocess inherit the parent's groups (gitGroup).
	if cred.NoSetGroups {
		t.Error("NoSetGroups must be false so supplementary groups get cleared")
	}
	if len(cred.Groups) != 0 {
		t.Errorf("Groups = %v, want empty (no supplementary groups granted)", cred.Groups)
	}
}

func TestLandlockBackend_WrapMulti_LookupErrSkipsCredential(t *testing.T) {
	sb := &LandlockBackend{
		selfExe: "/path/to/knot",
		lookup: func(string) (uint32, uint32, error) {
			return 0, 0, syscall.ENOENT
		},
	}
	cmd := exec.Command("git", "status")

	wrapped, err := sb.WrapMulti([]string{"/repos/a"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.SysProcAttr != nil && wrapped.SysProcAttr.Credential != nil {
		t.Error("lookup errored; Credential should not be set")
	}
}

func TestLandlockBackend_WrapMulti_LookupZeroSkipsCredential(t *testing.T) {
	// uid == 0 is treated as "don't drop" so we never accidentally drop to
	// root. Verify Credential isn't set in that case.
	sb := &LandlockBackend{
		selfExe: "/path/to/knot",
		lookup: func(string) (uint32, uint32, error) {
			return 0, 0, nil
		},
	}
	cmd := exec.Command("git", "status")

	wrapped, err := sb.WrapMulti([]string{"/repos/a"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.SysProcAttr != nil && wrapped.SysProcAttr.Credential != nil {
		t.Error("lookup returned uid=0; Credential should not be set")
	}
}
