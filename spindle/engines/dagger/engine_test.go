package dagger

import (
	"context"
	"runtime"
	"strings"
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/models"
)

func testEngine(t *testing.T, cfg *config.Config) *Engine {
	t.Helper()
	e, err := New(context.Background(), cfg)
	if err != nil {
		t.Fatal(err)
	}
	return e
}

func TestNewDefersDockerClientUntilWorkflowSetup(t *testing.T) {
	t.Setenv("DOCKER_HOST", "tcp://127.0.0.1:2376")
	t.Setenv("DOCKER_TLS_VERIFY", "1")
	t.Setenv("DOCKER_CERT_PATH", t.TempDir())

	e := testEngine(t, &config.Config{})
	if e.docker != nil {
		t.Fatal("docker client initialized during engine initialization")
	}
	if _, err := e.ensureDocker(); err == nil {
		t.Fatal("expected incomplete Docker TLS configuration to fail when first used")
	}
}

func TestSetupWorkflowWithoutRunner(t *testing.T) {
	e := testEngine(t, &config.Config{})

	wf := &models.Workflow{Data: addlFields{}}
	err := e.SetupWorkflow(context.Background(), models.WorkflowId{}, wf, models.NullLogger{})
	if err == nil {
		t.Fatal("expected setup to fail with neither a runner host nor a docker socket")
	}
	if !strings.Contains(err.Error(), ErrNoRunner.Error()) {
		t.Fatalf("expected an ErrNoRunner explanation, got %v", err)
	}
}

func TestWorkflowImage(t *testing.T) {
	arch := ""
	if runtime.GOARCH == "arm64" {
		arch = "arm64/"
	}

	tests := []struct {
		name string
		cfg  config.DaggerPipelines
		deps []string
		want string
	}{
		{
			name: "baseline packages",
			cfg:  config.DaggerPipelines{Nixery: "nixery.tangled.sh"},
			want: "nixery.tangled.sh/" + arch + "bash/git/coreutils/curl/gnutar/gzip/docker-client",
		},
		{
			name: "dependencies are appended",
			cfg:  config.DaggerPipelines{Nixery: "nixery.tangled.sh"},
			deps: []string{"go", "nodejs"},
			want: "nixery.tangled.sh/" + arch + "bash/git/coreutils/curl/gnutar/gzip/docker-client/go/nodejs",
		},
		{
			name: "a dependency already in the baseline is not repeated",
			cfg:  config.DaggerPipelines{Nixery: "nixery.tangled.sh"},
			deps: []string{"git", "go", "go"},
			want: "nixery.tangled.sh/" + arch + "bash/git/coreutils/curl/gnutar/gzip/docker-client/go",
		},
		{
			name: "an explicit image wins over nixery and dependencies",
			cfg:  config.DaggerPipelines{Nixery: "nixery.tangled.sh", Image: "ghcr.io/acme/dagger:v1"},
			deps: []string{"go"},
			want: "ghcr.io/acme/dagger:v1",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := testEngine(t, &config.Config{DaggerPipelines: tt.cfg})
			if got := e.workflowImage(tt.deps); got != tt.want {
				t.Errorf("workflowImage() = %q, want %q", got, tt.want)
			}
		})
	}
}

const manifest = `
engine: dagger
module: ci
dependencies:
  - go
environment:
  GLOBAL: yes
steps:
  - name: Build
    command: build --source=.
  - name: Test
    command: test
    environment:
      LOCAL: yes
`

func TestInitWorkflow(t *testing.T) {
	e := testEngine(t, &config.Config{})

	twf := tangled.Pipeline_Workflow{Name: "ci.yml", Raw: manifest}
	tpl := tangled.Pipeline{
		TriggerMetadata: &tangled.Pipeline_TriggerMetadata{
			Kind: "push",
			Push: &tangled.Pipeline_PushTriggerData{NewSha: "deadbeef"},
			Repo: &tangled.Pipeline_TriggerRepo{Knot: "knot.example.com"},
		},
	}

	wf, err := e.InitWorkflow(twf, tpl)
	if err != nil {
		t.Fatal(err)
	}

	if wf.Name != "ci.yml" {
		t.Errorf("Name = %q, want %q", wf.Name, "ci.yml")
	}
	if wf.Environment["GLOBAL"] != "yes" {
		t.Errorf("workflow environment = %v, want GLOBAL set", wf.Environment)
	}

	addl, ok := wf.Data.(addlFields)
	if !ok {
		t.Fatalf("Data = %T, want addlFields", wf.Data)
	}
	if addl.module != "ci" {
		t.Errorf("module = %q, want %q", addl.module, "ci")
	}
	if !strings.HasSuffix(addl.image, "/go") {
		t.Errorf("image = %q, want the `go` dependency appended", addl.image)
	}

	// clone, install, detect and link are prepended, in that order, ahead of
	// the two user steps
	wantNames := []string{
		"Clone repository into workspace",
		"Install Dagger",
		"Detect Dagger module",
		"Link Dagger functions",
		"Build",
		"Test",
	}
	if len(wf.Steps) != len(wantNames) {
		t.Fatalf("got %d steps, want %d: %v", len(wf.Steps), len(wantNames), stepNames(wf.Steps))
	}
	for i, want := range wantNames {
		if got := wf.Steps[i].Name(); got != want {
			t.Errorf("step %d = %q, want %q", i, got, want)
		}
	}
	for i := range 4 {
		if kind := wf.Steps[i].Kind(); kind != models.StepKindSystem {
			t.Errorf("setup step %d has kind %v, want StepKindSystem", i, kind)
		}
	}

	// a user step's command is passed through untouched: the shims, not a
	// rewrite, are what turn `build` into a dagger call
	build := wf.Steps[4]
	if build.Kind() != models.StepKindUser {
		t.Errorf("step %q has kind %v, want StepKindUser", build.Name(), build.Kind())
	}
	if build.Command() != "build --source=." {
		t.Errorf("command = %q, want it unmodified", build.Command())
	}
	if env := wf.Steps[5].(Step).environment; env["LOCAL"] != "yes" {
		t.Errorf("step environment = %v, want LOCAL set", env)
	}
}

func TestInitWorkflowResolvesVersion(t *testing.T) {
	tests := []struct {
		name    string
		raw     string
		cfg     string
		want    string
		wantEnv bool
	}{
		{
			name: "neither pins a version, so the installer takes the latest",
			raw:  "engine: dagger\n",
		},
		{
			name:    "the workflow pins a version",
			raw:     "engine: dagger\nversion: 0.18.0\n",
			want:    "0.18.0",
			wantEnv: true,
		},
		{
			name:    "the operator default applies when the workflow is silent",
			raw:     "engine: dagger\n",
			cfg:     "0.17.2",
			want:    "0.17.2",
			wantEnv: true,
		},
		{
			name:    "the workflow overrides the operator default",
			raw:     "engine: dagger\nversion: 0.18.0\n",
			cfg:     "0.17.2",
			want:    "0.18.0",
			wantEnv: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			e := testEngine(t, &config.Config{DaggerPipelines: config.DaggerPipelines{Version: tt.cfg}})

			wf, err := e.InitWorkflow(tangled.Pipeline_Workflow{Name: "ci.yml", Raw: tt.raw}, tangled.Pipeline{})
			if err != nil {
				t.Fatal(err)
			}

			addl := wf.Data.(addlFields)
			if addl.version != tt.want {
				t.Errorf("version = %q, want %q", addl.version, tt.want)
			}

			// the install step reads the version off the environment
			got, ok := envMap(e.daggerEnvs(addl))[versionEnv]
			if ok != tt.wantEnv {
				t.Fatalf("%s present = %v, want %v", versionEnv, ok, tt.wantEnv)
			}
			if ok && got != tt.want {
				t.Errorf("%s = %q, want %q", versionEnv, got, tt.want)
			}
		})
	}
}

func TestInitWorkflowRejectsUnknownField(t *testing.T) {
	e := testEngine(t, &config.Config{})

	twf := tangled.Pipeline_Workflow{Name: "ci.yml", Raw: "engine: dagger\nmodules: ci\n"}
	if _, err := e.InitWorkflow(twf, tangled.Pipeline{}); err == nil {
		t.Fatal("expected `modules` to be reported as an unknown field")
	}
}

func TestInitWorkflowWithoutTriggerMetadata(t *testing.T) {
	e := testEngine(t, &config.Config{})

	twf := tangled.Pipeline_Workflow{Name: "ci.yml", Raw: manifest}
	wf, err := e.InitWorkflow(twf, tangled.Pipeline{})
	if err != nil {
		t.Fatal(err)
	}
	// no trigger to clone from, so the module steps still lead but the clone
	// step is gone rather than panicking on a nil trigger
	if got := wf.Steps[0].Name(); got != "Install Dagger" {
		t.Errorf("first step = %q, want the install step", got)
	}
}

func TestDaggerEnvs(t *testing.T) {
	e := testEngine(t, &config.Config{
		Server: config.Server{DockerSocket: "/var/run/docker.sock"},
		DaggerPipelines: config.DaggerPipelines{
			RunnerHost: "tcp://dagger:8080",
			CloudToken: "tok",
		},
	})

	envs := envMap(e.daggerEnvs(addlFields{module: "ci"}))

	path, ok := envs["PATH"]
	if !ok {
		t.Fatal("PATH not set")
	}
	if !strings.HasPrefix(path, shimDir+":"+cliDir+":") {
		t.Errorf("PATH = %q, want the shim dir then the cli dir at the front", path)
	}

	want := map[string]string{
		"HOME":                             homeDir,
		"NO_COLOR":                         "1",
		"DAGGER_NO_NAG":                    "1",
		moduleEnv:                          "ci",
		"_EXPERIMENTAL_DAGGER_RUNNER_HOST": "tcp://dagger:8080",
		"DAGGER_CLOUD_TOKEN":               "tok",
		"DOCKER_HOST":                      "unix:///var/run/docker.sock",
	}
	for k, v := range want {
		if envs[k] != v {
			t.Errorf("%s = %q, want %q", k, envs[k], v)
		}
	}
}

func TestDaggerEnvsOmitsUnsetOptions(t *testing.T) {
	e := testEngine(t, &config.Config{})

	envs := envMap(e.daggerEnvs(addlFields{}))

	for _, k := range []string{moduleEnv, versionEnv, "_EXPERIMENTAL_DAGGER_RUNNER_HOST", "DAGGER_CLOUD_TOKEN", "DOCKER_HOST"} {
		if v, ok := envs[k]; ok {
			t.Errorf("%s set to %q, want it absent", k, v)
		}
	}
}

func TestWorkflowTimeout(t *testing.T) {
	e := testEngine(t, &config.Config{DaggerPipelines: config.DaggerPipelines{WorkflowTimeout: "20m"}})
	if got := e.WorkflowTimeout().String(); got != "20m0s" {
		t.Errorf("WorkflowTimeout() = %s, want 20m0s", got)
	}

	// an unparseable timeout falls back rather than running unbounded
	e = testEngine(t, &config.Config{DaggerPipelines: config.DaggerPipelines{WorkflowTimeout: "soon"}})
	if got := e.WorkflowTimeout().String(); got != "15m0s" {
		t.Errorf("WorkflowTimeout() = %s, want the 15m0s fallback", got)
	}
}

func stepNames(steps []models.Step) []string {
	names := make([]string, len(steps))
	for i, s := range steps {
		names[i] = s.Name()
	}
	return names
}

func envMap(envs EnvVars) map[string]string {
	m := make(map[string]string, len(envs))
	for _, e := range envs {
		k, v, _ := strings.Cut(e, "=")
		m[k] = v
	}
	return m
}
