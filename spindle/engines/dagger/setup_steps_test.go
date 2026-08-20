package dagger

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func stubDagger(t *testing.T, functions string) string {
	t.Helper()

	bin := t.TempDir()
	script := fmt.Sprintf(`#!/bin/sh
case "$*" in
	version)
		echo "dagger v0.18.0" ;;
	*functions*)
		if [ -z '%s' ]; then
			echo "module load failed" >&2
			exit 1
		fi
		cat <<'TABLE'
%s
TABLE
		;;
	*)
		echo "unexpected dagger invocation: $*" >&2
		exit 1 ;;
esac
`, functions, functions)

	if err := os.WriteFile(filepath.Join(bin, "dagger"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	return bin
}

type scriptResult struct {
	stdout string
	stderr string
	err    error
}

func repo(t *testing.T, files map[string]string) string {
	t.Helper()

	dir := t.TempDir()
	for name, contents := range files {
		full := filepath.Join(dir, name)
		if strings.HasSuffix(name, "/") {
			if err := os.MkdirAll(full, 0o755); err != nil {
				t.Fatal(err)
			}
			continue
		}
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(contents), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

func stubCurl(t *testing.T) string {
	t.Helper()

	bin := t.TempDir()
	script := `#!/bin/sh
cat <<'INSTALLER'
set -eu
version="${DAGGER_VERSION:-9.9.9}"
mkdir -p "$BIN_DIR"
cat > "$BIN_DIR/dagger" <<EOF
#!/bin/sh
echo "dagger v$version (registry.dagger.io/engine) linux/amd64"
EOF
chmod +x "$BIN_DIR/dagger"
INSTALLER
`
	if err := os.WriteFile(filepath.Join(bin, "curl"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	return bin
}

func TestInstallDagger(t *testing.T) {
	tests := []struct {
		name string
		// version the workflow (or the operator) pinned
		want string
		// version the image already ships, empty for an image without dagger
		preinstalled string
		wantInstall  bool
		wantVersion  string
	}{
		{
			name:        "no version pinned and no cli in the image installs the latest",
			wantInstall: true,
			wantVersion: "9.9.9", // what the stub installer calls "latest"
		},
		{
			name:         "no version pinned leaves the image's own cli alone",
			preinstalled: "0.16.0",
			wantVersion:  "0.16.0",
		},
		{
			name:        "a pinned version is installed",
			want:        "0.18.0",
			wantInstall: true,
			wantVersion: "0.18.0",
		},
		{
			name:         "a pinned version replaces a mismatched cli in the image",
			want:         "0.18.0",
			preinstalled: "0.16.0",
			wantInstall:  true,
			wantVersion:  "0.18.0",
		},
		{
			name:         "a pinned version already in the image is not reinstalled",
			want:         "0.16.0",
			preinstalled: "0.16.0",
			wantVersion:  "0.16.0",
		},
		{
			// `version: v0.18.0` is the same request as `version: 0.18.0`
			name:         "a leading v is not part of the version",
			want:         "v0.16.0",
			preinstalled: "0.16.0",
			wantVersion:  "0.16.0",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			state := t.TempDir()
			path := stubCurl(t)
			if tt.preinstalled != "" {
				// a dagger that only answers `version`, standing in for one
				// baked into the image
				pre := t.TempDir()
				script := fmt.Sprintf("#!/bin/sh\necho \"dagger v%s (registry.dagger.io/engine) linux/amd64\"\n", tt.preinstalled)
				if err := os.WriteFile(filepath.Join(pre, "dagger"), []byte(script), 0o755); err != nil {
					t.Fatal(err)
				}
				path = pre + ":" + path
			}

			env := []string{"PATH=" + path + ":" + binWith(t, "mkdir", "cat", "chmod", "sh")}
			if tt.want != "" {
				env = append(env, versionEnv+"="+tt.want)
			}

			res := runStepWithPath(t, installDaggerStep(), t.TempDir(), state, env)
			if res.err != nil {
				t.Fatalf("install failed: %v\nstdout: %s\nstderr: %s", res.err, res.stdout, res.stderr)
			}

			installed := filepath.Join(state, "cli", "dagger")
			if _, err := os.Stat(installed); (err == nil) != tt.wantInstall {
				t.Errorf("cli installed = %v, want %v (stdout: %s)", err == nil, tt.wantInstall, res.stdout)
			}
			if !strings.Contains(res.stdout, tt.wantVersion) {
				t.Errorf("stdout = %q, want it to report version %s", res.stdout, tt.wantVersion)
			}
		})
	}
}

func TestInstallDaggerFailure(t *testing.T) {
	bin := t.TempDir()
	// an installer that runs but leaves no cli behind
	if err := os.WriteFile(filepath.Join(bin, "curl"), []byte("#!/bin/sh\necho 'echo installing...'\n"), 0o755); err != nil {
		t.Fatal(err)
	}

	state := t.TempDir()
	res := runStepWithPath(t, installDaggerStep(), t.TempDir(), state,
		[]string{"PATH=" + bin + ":" + binWith(t, "mkdir", "sh")})

	if res.err == nil {
		t.Fatal("expected a failed install to fail the step")
	}
	if !strings.Contains(res.stderr, "did not produce a cli") {
		t.Errorf("stderr = %q, want the failed install surfaced", res.stderr)
	}
}

func TestDetectModule(t *testing.T) {
	tests := []struct {
		name    string
		files   map[string]string
		module  string
		want    string
		wantErr string
	}{
		{
			// what `dagger init` produces: config at the root, sources in
			// .dagger. `dagger call -m` wants the dagger.json directory
			name:  "dagger.json at the root with a .dagger source directory",
			files: map[string]string{"dagger.json": "{}", ".dagger/main.go": "package main"},
			want:  ".",
		},
		{
			name:  "module contained entirely in .dagger",
			files: map[string]string{".dagger/dagger.json": "{}"},
			want:  ".dagger",
		},
		{
			name:  "a .dagger directory with no config anywhere",
			files: map[string]string{".dagger/": ""},
			want:  ".dagger",
		},
		{
			name:   "an explicit module wins over the root config",
			files:  map[string]string{"dagger.json": "{}", "ci/dagger.json": "{}"},
			module: "ci",
			want:   "ci",
		},
		{
			name:    "an explicit module without a config is an error",
			files:   map[string]string{"dagger.json": "{}", "ci/": ""},
			module:  "ci",
			wantErr: "no dagger.json in \"ci\"",
		},
		{
			name:    "a repository with no module at all is an error",
			files:   map[string]string{"README.md": "hi"},
			wantErr: "no dagger module found",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			workspace := repo(t, tt.files)
			state := t.TempDir()

			env := []string{"PATH=" + stubDagger(t, "Name\nbuild\n") + ":" + os.Getenv("PATH")}
			if tt.module != "" {
				env = append(env, moduleEnv+"="+tt.module)
			}

			res := runStepWithPath(t, detectModuleStep(), workspace, state, env)

			if tt.wantErr != "" {
				if res.err == nil {
					t.Fatalf("expected failure, got stdout %q", res.stdout)
				}
				if !strings.Contains(res.stderr, tt.wantErr) {
					t.Errorf("stderr = %q, want it to mention %q", res.stderr, tt.wantErr)
				}
				return
			}
			if res.err != nil {
				t.Fatalf("detect failed: %v\nstderr: %s", res.err, res.stderr)
			}

			got, err := os.ReadFile(filepath.Join(state, "module"))
			if err != nil {
				t.Fatal(err)
			}
			if string(got) != tt.want {
				t.Errorf("resolved module = %q, want %q", got, tt.want)
			}
			if !strings.Contains(res.stdout, "dagger module: "+tt.want) {
				t.Errorf("stdout = %q, want the resolved module reported", res.stdout)
			}
		})
	}
}

// binWith builds a directory holding just the named commands, so a test can
// hand the script a PATH that is missing dagger but still usable.
func binWith(t *testing.T, cmds ...string) string {
	t.Helper()

	bin := t.TempDir()
	for _, c := range cmds {
		target, err := exec.LookPath(c)
		if err != nil {
			t.Skipf("%s not available", c)
		}
		if err := os.Symlink(target, filepath.Join(bin, c)); err != nil {
			t.Fatal(err)
		}
	}
	return bin
}

func TestDetectModuleWithoutDaggerCLI(t *testing.T) {
	workspace := repo(t, map[string]string{"dagger.json": "{}"})
	state := t.TempDir()

	// everything the script needs is on PATH except dagger itself
	res := runStepWithPath(t, detectModuleStep(), workspace, state,
		[]string{"PATH=" + binWith(t, "mkdir")})

	if res.err == nil {
		t.Fatal("expected the missing dagger cli to fail the step")
	}
	if !strings.Contains(res.stderr, "dagger cli is not present") {
		t.Errorf("stderr = %q, want it to name the missing cli", res.stderr)
	}
}

const functionTable = `Name            Description
build           Build the project
integration-test Run the integration suite
publish         Publish a release
`

func TestLinkFunctions(t *testing.T) {
	state := t.TempDir()
	if err := os.WriteFile(filepath.Join(state, "module"), []byte("."), 0o644); err != nil {
		t.Fatal(err)
	}

	workspace := repo(t, map[string]string{"dagger.json": "{}"})
	res := runStepWithPath(t, linkFunctionsStep(), workspace, state,
		[]string{"PATH=" + stubDagger(t, functionTable) + ":" + os.Getenv("PATH")})
	if res.err != nil {
		t.Fatalf("link failed: %v\nstderr: %s", res.err, res.stderr)
	}

	shims := filepath.Join(state, "bin")
	for _, fn := range []string{"build", "integration-test", "publish"} {
		info, err := os.Stat(filepath.Join(shims, fn))
		if err != nil {
			t.Fatalf("no shim for %q: %v", fn, err)
		}
		if info.Mode().Perm()&0o111 == 0 {
			t.Errorf("shim %q is not executable (mode %v)", fn, info.Mode())
		}
	}

	// the header is not a function
	if _, err := os.Stat(filepath.Join(shims, "Name")); err == nil {
		t.Error("the table header was linked as a function")
	}

	got, err := os.ReadFile(filepath.Join(shims, "build"))
	if err != nil {
		t.Fatal(err)
	}
	want := "exec dagger --progress plain call -m . build \"$@\""
	if !strings.Contains(string(got), want) {
		t.Errorf("shim = %q, want it to contain %q", got, want)
	}

	if !strings.Contains(res.stdout, "linked 3 dagger function(s)") {
		t.Errorf("stdout = %q, want the linked count reported", res.stdout)
	}
}

// a shim must be a real command, so a function call composes with the rest of
// the step's shell the same way any other command does
func TestLinkedFunctionIsCallableByName(t *testing.T) {
	state := t.TempDir()
	if err := os.WriteFile(filepath.Join(state, "module"), []byte(".dagger"), 0o644); err != nil {
		t.Fatal(err)
	}

	stub := stubDagger(t, functionTable)
	workspace := repo(t, map[string]string{".dagger/dagger.json": "{}"})
	res := runStepWithPath(t, linkFunctionsStep(), workspace, state,
		[]string{"PATH=" + stub + ":" + os.Getenv("PATH")})
	if res.err != nil {
		t.Fatalf("link failed: %v\nstderr: %s", res.err, res.stderr)
	}

	// the stub rejects anything it does not recognise, so reaching `call`
	// through the shim is what proves the name resolved and the module and
	// arguments were forwarded
	userStep := Step{command: `build --source=. | cat`}
	res = runStepWithPath(t, userStep, workspace, state,
		[]string{"PATH=" + filepath.Join(state, "bin") + ":" + stub})
	if res.err == nil || !strings.Contains(res.stderr, "unexpected dagger invocation") {
		t.Fatalf("expected the shim to reach dagger; err %v, stderr %q", res.err, res.stderr)
	}
	if !strings.Contains(res.stderr, "call -m .dagger build --source=.") {
		t.Errorf("stderr = %q, want the module and arguments forwarded", res.stderr)
	}
}

func TestLinkFunctionsWithEmptyModule(t *testing.T) {
	state := t.TempDir()
	if err := os.WriteFile(filepath.Join(state, "module"), []byte("."), 0o644); err != nil {
		t.Fatal(err)
	}

	workspace := repo(t, map[string]string{"dagger.json": "{}"})
	res := runStepWithPath(t, linkFunctionsStep(), workspace, state,
		[]string{"PATH=" + stubDagger(t, "") + ":" + os.Getenv("PATH")})

	if res.err == nil {
		t.Fatal("expected a module that lists no functions to fail the step")
	}
	if !strings.Contains(res.stderr, "failed to list the functions") {
		t.Errorf("stderr = %q, want the listing failure surfaced", res.stderr)
	}
}

// runStepWithPath is runStep with the caller supplying the whole environment,
// PATH included.
func runStepWithPath(t *testing.T, step Step, workspace, state string, env []string) scriptResult {
	t.Helper()

	bash, err := exec.LookPath("bash")
	if err != nil {
		t.Skip("bash not available")
	}

	cmd := exec.Command(bash, "-c", step.Command())
	cmd.Dir = workspace
	cmd.Env = append([]string{dirEnv + "=" + state}, env...)

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	runErr := cmd.Run()

	return scriptResult{stdout: stdout.String(), stderr: stderr.String(), err: runErr}
}
