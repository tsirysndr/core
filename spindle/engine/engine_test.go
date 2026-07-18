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
	StartWorkflows(logger, nil, cfg, nil, testDB, nil, context.Background(), pipeline, pipelineId)

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

func TestCancelWorkflow_NotOverwritten(t *testing.T) {
	t.Parallel()

	testDB := newTestDB(t)
	logger := slog.New(slog.NewTextHandler(os.Stderr, nil))

	stepStarted := make(chan struct{})
	eng := &mockEngine{
		runStepFunc: func(ctx context.Context, wid models.WorkflowId, idx int) error {
			close(stepStarted)
			<-ctx.Done()
			return ctx.Err()
		},
	}

	pipelineId := models.PipelineId{
		Knot: "test-knot",
		Rkey: "test-rkey",
	}

	wid := models.WorkflowId{
		PipelineId: pipelineId,
		Name:       "cancel_test_job",
	}

	pipeline := &models.Pipeline{
		Workflows: map[models.Engine][]models.Workflow{
			eng: {
				{
					Name:  "cancel_test_job",
					Steps: []models.Step{mockStep{name: "step1"}},
				},
			},
		},
	}

	cfg := &config.Config{Server: config.Server{LogDir: t.TempDir()}}
	doneChan := make(chan struct{})
	go func() {
		StartWorkflows(logger, nil, cfg, nil, testDB, nil, context.Background(), pipeline, pipelineId)
		close(doneChan)
	}()

	select {
	case <-stepStarted:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for step to start")
	}

	_ = testDB.StatusCancelled(wid, "User canceled the workflow", -1, nil)
	CancelWorkflow(wid)

	select {
	case <-doneChan:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for StartWorkflows to complete")
	}

	// the runner writes StatusCancelled itself when it sees the canceled ctx
	// the handler writes nothing for a live wf, so nothing lands after to overwrite it
	st, err := testDB.GetStatus(wid)
	if err != nil {
		t.Fatalf("GetStatus error = %v", err)
	}
	if st.Status != string(models.StatusKindCancelled) {
		t.Fatalf("expected status to be cancelled, got %s", st.Status)
	}
}

func TestSetupTimeout_ReportsTimeout(t *testing.T) {
	t.Parallel()

	testDB := newTestDB(t)
	logger := slog.New(slog.NewTextHandler(os.Stderr, nil))

	// setup blocks past the workflow timeout, so it should land as timeout not failed
	eng := &mockEngine{
		timeout: 100 * time.Millisecond,
		setupFunc: func(ctx context.Context, wid models.WorkflowId) error {
			<-ctx.Done()
			return ctx.Err()
		},
	}

	pipelineId := models.PipelineId{Knot: "test-knot", Rkey: "test-rkey"}
	wid := models.WorkflowId{PipelineId: pipelineId, Name: "timeout_job"}

	pipeline := &models.Pipeline{
		Workflows: map[models.Engine][]models.Workflow{
			eng: {{Name: "timeout_job", Steps: []models.Step{mockStep{name: "step1"}}}},
		},
	}

	cfg := &config.Config{Server: config.Server{LogDir: t.TempDir()}}
	StartWorkflows(logger, nil, cfg, nil, testDB, nil, context.Background(), pipeline, pipelineId)

	st, err := testDB.GetStatus(wid)
	if err != nil {
		t.Fatalf("GetStatus error = %v", err)
	}
	if st.Status != string(models.StatusKindTimeout) {
		t.Fatalf("expected status to be timeout, got %s", st.Status)
	}

	if len(eng.runStepCalls) != 0 {
		t.Fatalf("expected no steps to run after setup timeout, got %d", len(eng.runStepCalls))
	}
}
