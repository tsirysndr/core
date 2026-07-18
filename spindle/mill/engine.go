package mill

import (
	"context"
	"log/slog"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

// raw pipeline/workflow carried forward, executor runs the real InitWorkflow
type millWorkflowState struct {
	RawWorkflow tangled.Pipeline_Workflow
	RawPipeline tangled.Pipeline
	Lease       *RemoteLease
}

// stand-in for a real engine, registered under the real names
// ("microvm", "nixery"), all sharing one Mill
type Engine struct {
	name string
	mill *Mill
	l    *slog.Logger
}

func (e *Engine) AuthorsRemoteStatus() {}

func NewEngine(name string, mill *Mill) *Engine {
	return &Engine{name: name, mill: mill, l: mill.l.With("engine", "mill:"+name)}
}

// synthetic one-step workflow so processPipeline injects TANGLED_* env
// and marks pending normally. the real InitWorkflow runs exactly once, on
// the executor inside ReserveSeat, and commit reuses that workflow
func (e *Engine) InitWorkflow(twf tangled.Pipeline_Workflow, tpl tangled.Pipeline) (*models.Workflow, error) {
	return &models.Workflow{
		Name:        twf.Name,
		Environment: map[string]string{},
		Steps:       []models.Step{remoteStep{}},
		Data: &millWorkflowState{
			RawWorkflow: twf,
			RawPipeline: tpl,
		},
	}, nil
}

// no-op logger for the synthetic workflow. the executor's lines stream
// into this wid's log directly, a local logger would just write competing
// lines
func (e *Engine) WorkflowLogger(wid models.WorkflowId) models.WorkflowLogger {
	return models.NullLogger{}
}

// the placement seam, blocks on remote placement which the user sees as
// "pending". only StartWorkflows calls this, always Wait
func (e *Engine) AcquireWorkflowSlot(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, _ engine.AcquireMode) (engine.WorkflowSlot, error) {
	return e.mill.place(ctx, e.name, wid, wf)
}

// real setup happens on the executor
func (e *Engine) SetupWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) error {
	e.l.Info("remote job placed, awaiting commit", "wid", wid)
	return nil
}

// hands over the secrets and blocks on the terminal result streamed over the
// session
func (e *Engine) RunStep(ctx context.Context, wid models.WorkflowId, w *models.Workflow, idx int, unlocked []secrets.UnlockedSecret, wfLogger models.WorkflowLogger) error {
	return e.mill.commitAndWait(ctx, w, unlocked)
}

// deliberately generous, the executor enforces the real timeout. the mill
// only caps a hung or silent executor, true death is caught by reconnect grace
func (e *Engine) WorkflowTimeout() time.Duration {
	return e.mill.cfg.JobTimeout
}

// cancels a still-running attempt. no-op if already terminal
func (e *Engine) DestroyWorkflow(ctx context.Context, wid models.WorkflowId) error {
	e.mill.destroy(wid)
	return nil
}
