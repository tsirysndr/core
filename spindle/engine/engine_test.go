package engine

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

type mockStep struct {
	name    string
	command string
}

func (m mockStep) Name() string          { return m.name }
func (m mockStep) Command() string       { return m.command }
func (m mockStep) Kind() models.StepKind { return models.StepKindUser }

type mockEngine struct {
	mu           sync.Mutex
	setupCalls   []models.WorkflowId
	runStepCalls []models.WorkflowId
	setupFunc    func(ctx context.Context, wid models.WorkflowId) error
	runStepFunc  func(ctx context.Context, wid models.WorkflowId, idx int) error
	timeout      time.Duration
}

func (m *mockEngine) InitWorkflow(twf tangled.Pipeline_Workflow, tpl tangled.Pipeline) (*models.Workflow, error) {
	return &models.Workflow{}, nil
}

func (m *mockEngine) SetupWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) error {
	m.mu.Lock()
	m.setupCalls = append(m.setupCalls, wid)
	fn := m.setupFunc
	m.mu.Unlock()
	if fn != nil {
		return fn(ctx, wid)
	}
	return nil
}

func (m *mockEngine) WorkflowTimeout() time.Duration {
	if m.timeout != 0 {
		return m.timeout
	}
	return 5 * time.Second
}

func (m *mockEngine) DestroyWorkflow(ctx context.Context, wid models.WorkflowId) error {
	return nil
}

func (m *mockEngine) RunStep(ctx context.Context, wid models.WorkflowId, w *models.Workflow, idx int, secrets []secrets.UnlockedSecret, wfLogger models.WorkflowLogger) error {
	m.mu.Lock()
	m.runStepCalls = append(m.runStepCalls, wid)
	fn := m.runStepFunc
	m.mu.Unlock()

	if fn != nil {
		return fn(ctx, wid, idx)
	}
	return nil
}

func newTestDB(t *testing.T) *db.DB {
	t.Helper()
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatalf("failed to create test db: %v", err)
	}
	return d
}

func TestStartWorkflows_CollisionRejection(t *testing.T) {
	t.Parallel()

	testDB := newTestDB(t)
	logger := slog.New(slog.NewTextHandler(os.Stderr, nil))

	eng := &mockEngine{}
	pipelineId := models.PipelineId{
		Knot: "test-knot",
		Rkey: "test-rkey",
	}

	// two names that normalize to the same wid must not both run
	wfColliding1 := models.Workflow{
		Name:  "test-job",
		Steps: []models.Step{mockStep{name: "step1"}},
	}
	wfColliding2 := models.Workflow{
		Name:  "test job",
		Steps: []models.Step{mockStep{name: "step1"}},
	}
	wfUnique := models.Workflow{
		Name:  "unique_job",
		Steps: []models.Step{mockStep{name: "step1"}},
	}

	pipeline := &models.Pipeline{
		Workflows: map[models.Engine][]models.Workflow{
			eng: {wfColliding1, wfColliding2, wfUnique},
		},
	}

	cfg := &config.Config{Server: config.Server{LogDir: t.TempDir()}}
	StartWorkflows(logger, nil, cfg, testDB, nil, context.Background(), pipeline, pipelineId)

	eng.mu.Lock()
	setupCalls := append([]models.WorkflowId(nil), eng.setupCalls...)
	eng.mu.Unlock()

	for _, call := range setupCalls {
		if call.Name == "test-job" || call.Name == "test job" {
			t.Fatalf("expected colliding workflow %s to not be started", call.Name)
		}
	}

	hasUnique := false
	for _, call := range setupCalls {
		if call.Name == "unique_job" {
			hasUnique = true
		}
	}
	if !hasUnique {
		t.Fatalf("expected unique workflow unique_job to be started")
	}

	widColliding1 := models.WorkflowId{PipelineId: pipelineId, Name: "test-job"}
	widColliding2 := models.WorkflowId{PipelineId: pipelineId, Name: "test job"}
	widUnique := models.WorkflowId{PipelineId: pipelineId, Name: "unique_job"}

	status1, err := testDB.GetStatus(widColliding1)
	if err != nil || status1.Status != string(models.StatusKindFailed) {
		t.Fatalf("expected colliding1 status to be failed, got status=%v err=%v", status1, err)
	}

	status2, err := testDB.GetStatus(widColliding2)
	if err != nil || status2.Status != string(models.StatusKindFailed) {
		t.Fatalf("expected colliding2 status to be failed, got status=%v err=%v", status2, err)
	}

	statusUnique, err := testDB.GetStatus(widUnique)
	if err != nil || statusUnique.Status != string(models.StatusKindSuccess) {
		t.Fatalf("expected unique status to be success, got status=%v err=%v", statusUnique, err)
	}
}
