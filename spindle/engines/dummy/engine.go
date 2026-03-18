package dummy

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	"gopkg.in/yaml.v3"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

// DummyEngine is a no-op engine that logs all lifecycle events via slog and writes
// step output to the workflow logger. Useful for testing pipeline plumbing
// without a real execution backend.
type DummyEngine struct {
	l *slog.Logger
}

func New(l *slog.Logger) *DummyEngine {
	return &DummyEngine{l: l.With("engine", "dummy")}
}

type Step struct {
	name    string
	kind    models.StepKind
	command string
}

func (s Step) Name() string          { return s.name }
func (s Step) Command() string       { return s.command }
func (s Step) Kind() models.StepKind { return s.kind }

func (e *DummyEngine) InitWorkflow(twf tangled.Pipeline_Workflow, _ tangled.Pipeline) (*models.Workflow, error) {
	dwf := &struct {
		Steps []struct {
			Name    string `yaml:"name"`
			Command string `yaml:"command"`
		} `yaml:"steps"`
		Environment map[string]string `yaml:"environment"`
	}{}

	if err := yaml.Unmarshal([]byte(twf.Raw), dwf); err != nil {
		return nil, err
	}

	wf := &models.Workflow{
		Name:        twf.Name,
		Environment: dwf.Environment,
	}
	for _, ds := range dwf.Steps {
		wf.Steps = append(wf.Steps, Step{
			name:    ds.Name,
			kind:    models.StepKindUser,
			command: ds.Command,
		})
	}

	e.l.Info("workflow initialised", "name", twf.Name, "steps", len(wf.Steps))
	return wf, nil
}

func (e *DummyEngine) SetupWorkflow(_ context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) error {
	e.l.Info("setting up workflow", "wid", wid)

	setupStep := Step{name: "dummy setup", kind: models.StepKindSystem}
	const setupIdx = -1

	wfLogger.ControlWriter(setupIdx, setupStep, models.StepStatusStart).Write([]byte{0})
	defer wfLogger.ControlWriter(setupIdx, setupStep, models.StepStatusEnd).Write([]byte{0})

	fmt.Fprintf(wfLogger.DataWriter(setupIdx, "stdout"), "dummy engine: workflow %q ready", wf.Name)
	return nil
}

func (e *DummyEngine) WorkflowTimeout() time.Duration {
	return 5 * time.Minute
}

func (e *DummyEngine) DestroyWorkflow(_ context.Context, wid models.WorkflowId) error {
	e.l.Info("destroying workflow", "wid", wid)
	return nil
}

func (e *DummyEngine) RunStep(_ context.Context, wid models.WorkflowId, w *models.Workflow, idx int, _ []secrets.UnlockedSecret, wfLogger models.WorkflowLogger) error {
	step := w.Steps[idx]
	e.l.Info("running step", "wid", wid, "step", step.Name(), "command", step.Command())
	fmt.Fprintf(wfLogger.DataWriter(idx, "stdout"), "$ %s", step.Command())
	return nil
}
