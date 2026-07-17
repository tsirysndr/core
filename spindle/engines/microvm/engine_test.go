package microvm

import (
	"context"
	"encoding/json"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
)

func writeTestImageSpec(t *testing.T, dir, name string, spec ImageSpec) {
	t.Helper()
	data, err := json.Marshal(spec)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, name+".json"), data, 0o644); err != nil {
		t.Fatal(err)
	}
}

func testEngine(t *testing.T, imageDir string) *Engine {
	t.Helper()
	return &Engine{
		l: slog.Default(),
		cfg: &config.Config{
			MicroVMPipelines: config.MicroVMPipelines{
				ImageDir:     imageDir,
				DefaultImage: "alpine",
			},
		},
	}
}

func TestNewDefersAgentHubUntilWorkflowSetup(t *testing.T) {
	e, err := New(context.Background(), &config.Config{}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if e.agent != nil {
		t.Fatal("agent hub started during engine initialization")
	}
}

func TestInitWorkflowRejectsConfigOnNonNixOSImage(t *testing.T) {
	dir := t.TempDir()
	writeTestImageSpec(t, dir, "alpine", validImageSpec())

	e := testEngine(t, dir)
	_, err := e.InitWorkflow(tangled.Pipeline_Workflow{
		Raw: `
image: alpine
dependencies:
  - nixpkgs#hello
steps:
  - name: hello
    command: hello
`,
	}, tangled.Pipeline{})
	if err == nil {
		t.Fatal("expected error for NixOS config options on a non-NixOS image")
	}
	if !strings.Contains(err.Error(), "NixOS") {
		t.Fatalf("error should mention NixOS images, got: %v", err)
	}
}

func TestInitWorkflowPlainStepsOnNonNixOSImage(t *testing.T) {
	dir := t.TempDir()
	writeTestImageSpec(t, dir, "alpine", validImageSpec())

	e := testEngine(t, dir)
	wf, err := e.InitWorkflow(tangled.Pipeline_Workflow{
		Raw: `
image: alpine
steps:
  - name: hello
    command: echo hello
`,
	}, tangled.Pipeline{})
	if err != nil {
		t.Fatal(err)
	}
	if len(wf.Steps) != 1 {
		t.Fatalf("expected exactly the user step, got %d steps", len(wf.Steps))
	}
	state, ok := wf.Data.(*workflowState)
	if !ok {
		t.Fatal("workflow data is not workflowState")
	}
	if state.ConfigKey != "" {
		t.Fatalf("non-NixOS workflow should not have a config key, got %q", state.ConfigKey)
	}
}

func TestInitWorkflowConfigOnNixOSImage(t *testing.T) {
	dir := t.TempDir()
	spec := validImageSpec()
	spec.BaseConfigHash = "abcdef123456"
	writeTestImageSpec(t, dir, "nixos", spec)

	e := testEngine(t, dir)
	wf, err := e.InitWorkflow(tangled.Pipeline_Workflow{
		Raw: `
image: nixos
dependencies:
  - nixpkgs#hello
steps:
  - name: hello
    command: hello
`,
	}, tangled.Pipeline{})
	if err != nil {
		t.Fatal(err)
	}
	if len(wf.Steps) != 2 {
		t.Fatalf("expected activation step + user step, got %d steps", len(wf.Steps))
	}
	if step, ok := wf.Steps[0].(Step); !ok || step.action != activationStepAction {
		t.Fatalf("first step should be the activation step, got %+v", wf.Steps[0])
	}
}
