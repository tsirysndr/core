package dagger

import (
	"context"
	"io"
	"os"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/models"
)

// captureLogger records everything a workflow writes so the test can assert on
// what the steps actually printed.
type captureLogger struct {
	mu  sync.Mutex
	buf strings.Builder
}

func (l *captureLogger) Close() error { return nil }

func (l *captureLogger) DataWriter(_ int, _ string) io.Writer {
	return writerFunc(func(p []byte) (int, error) {
		l.mu.Lock()
		defer l.mu.Unlock()
		l.buf.Write(p)
		l.buf.WriteByte('\n')
		return len(p), nil
	})
}

func (l *captureLogger) ControlWriter(_ int, _ models.Step, _ models.StepStatus) io.Writer {
	return io.Discard
}

func (l *captureLogger) String() string {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.buf.String()
}

type writerFunc func([]byte) (int, error)

func (f writerFunc) Write(p []byte) (int, error) { return f(p) }

// the module `dagger init --sdk=go` scaffolds exposes container-echo, so these
// steps call a function the test never had to write itself
const e2eManifest = `
engine: dagger
steps:
  - name: call a function by name
    command: container-echo --string-arg=hello-from-tangled stdout

  - name: compose a function call with ordinary shell
    command: |
      container-echo --string-arg=piped stdout | tr 'a-z' 'A-Z'
`

// TestDaggerEngineEndToEnd drives the engine against a real Docker daemon and a
// real Dagger module: it pulls the workflow image, installs the cli, scaffolds
// a module, reads its function list, and calls one of those functions by name
// from a step. Nothing about dagger is stubbed, so a green run means the shims
// reached the real cli and the real cli reached a real engine.
//
// It is slow (tens of minutes on a cold host: the workflow image, the cli, the
// dagger engine image and the module's own toolchain all have to come down the
// wire) and needs network plus a writable docker socket, which is why it is
// opt-in.
//
// run with:
//
//	SPINDLE_DAGGER_INTEGRATION=1 go test -v -timeout 40m \
//	  -run TestDaggerEngineEndToEnd ./spindle/engines/dagger/
//
// SPINDLE_SERVER_DOCKER_SOCKET overrides the socket path, and
// SPINDLE_DAGGER_INTEGRATION_VERSION pins the cli version under test.
func TestDaggerEngineEndToEnd(t *testing.T) {
	if os.Getenv("SPINDLE_DAGGER_INTEGRATION") != "1" {
		t.Skip("see the doc comment on this test for how to run it")
	}

	socket := os.Getenv("SPINDLE_SERVER_DOCKER_SOCKET")
	if socket == "" {
		socket = "/var/run/docker.sock"
	}
	if _, err := os.Stat(socket); err != nil {
		t.Skipf("no docker socket at %s: %v", socket, err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Minute)
	defer cancel()

	e, err := New(ctx, &config.Config{
		Server: config.Server{DockerSocket: socket},
		DaggerPipelines: config.DaggerPipelines{
			Nixery:                 "nixery.tangled.sh",
			Version:                os.Getenv("SPINDLE_DAGGER_INTEGRATION_VERSION"),
			WorkflowTimeout:        "35m",
			MaxConcurrentWorkflows: 1,
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	// no trigger metadata, so InitWorkflow adds no clone step: the module is
	// scaffolded into the workspace below instead of coming from a repository
	wf, err := e.InitWorkflow(tangled.Pipeline_Workflow{Name: "e2e.yml", Raw: e2eManifest}, tangled.Pipeline{})
	if err != nil {
		t.Fatal(err)
	}

	// stands in for the clone, and has to land after the cli is installed but
	// before detection runs. `dagger init` writes the layout the engine is
	// expected to resolve to ".": dagger.json at the root, sources in .dagger
	scaffold := Step{
		name:    "Scaffold a Dagger module",
		kind:    models.StepKindSystem,
		command: "dagger init --sdk=go --source=.dagger --name=e2e",
	}
	wf.Steps = slices.Insert(wf.Steps, 1, models.Step(scaffold))

	wid := models.WorkflowId{
		PipelineId: models.PipelineId{Knot: "e2e.invalid", Rkey: "dagger-e2e"},
		Name:       "e2e.yml",
	}
	logger := &captureLogger{}

	if err := e.SetupWorkflow(ctx, wid, wf, logger); err != nil {
		t.Fatalf("setup: %v\n%s", err, logger)
	}
	t.Cleanup(func() {
		if err := e.DestroyWorkflow(context.Background(), wid); err != nil {
			t.Errorf("destroy: %v", err)
		}
	})

	for idx, step := range wf.Steps {
		t.Logf("step %d: %s", idx, step.Name())
		if err := e.RunStep(ctx, wid, wf, idx, nil, logger); err != nil {
			t.Fatalf("step %d (%s): %v\n%s", idx, step.Name(), err, logger)
		}
	}

	out := logger.String()
	for _, want := range []string{
		"dagger module: .",   // the root layout resolved the way it should
		"container-echo",     // the module's function list was read
		"linked",             // and turned into shims
		"hello-from-tangled", // a function reached by name, through a shim
		"PIPED",              // and composed with the rest of the step's shell
	} {
		if !strings.Contains(out, want) {
			t.Errorf("workflow output is missing %q\n--- output ---\n%s", want, out)
		}
	}
}
