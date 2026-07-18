package mill

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/notifier"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engines/dummy"
	"tangled.org/core/spindle/mill/executor"
	"tangled.org/core/spindle/models"
)

func TestEndToEndDummyJob(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	l := slog.New(slog.NewTextHandler(io.Discard, nil))

	millDir := t.TempDir()
	bdb, err := db.Make(ctx, filepath.Join(millDir, "mill.db"))
	if err != nil {
		t.Fatalf("mill db: %v", err)
	}
	bn := notifier.New()
	mill := New(l, Config{LogDir: millDir, ReconnectGrace: time.Minute, BidTimeout: 2 * time.Second})
	mill.Attach(bdb, &bn)
	if err := bdb.AddExecutorToken("exec-1", HashToken("test-token"), nil, nil); err != nil {
		t.Fatalf("register executor token: %v", err)
	}

	srv := httptest.NewServer(http.HandlerFunc(mill.HandleExecutorConn))
	defer srv.Close()
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")

	execDir := t.TempDir()
	edb, err := db.Make(ctx, filepath.Join(execDir, "exec.db"))
	if err != nil {
		t.Fatalf("exec db: %v", err)
	}
	en := notifier.New()
	cfg := &config.Config{}
	cfg.Server.Dev = true
	cfg.Server.LogDir = execDir
	cfg.ArtifactStores.Disk.Dir = filepath.Join(execDir, "artifacts")
	cfg.Server.Hostname = "exec-1"
	cfg.Mill.URL = wsURL
	cfg.Mill.Seats = 2
	cfg.Mill.SharedSecret = "test-token"

	dummyEng := dummy.New(l)
	dummyEng.StepDelay = 50 * time.Millisecond
	engines := map[string]models.Engine{"dummy": dummyEng}
	exec, err := executor.New(cfg, engines, edb, &en, l)
	if err != nil {
		t.Fatalf("executor.New: %v", err)
	}
	go exec.Connect(ctx)

	be := NewEngine("dummy", mill)
	twf := tangled.Pipeline_Workflow{
		Name: "build",
		Raw:  "steps:\n  - name: hello\n    command: echo hi\n",
	}
	wf, err := be.InitWorkflow(twf, tangled.Pipeline{TriggerMetadata: &tangled.Pipeline_TriggerMetadata{}})
	if err != nil {
		t.Fatalf("InitWorkflow: %v", err)
	}
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "knot.test", Rkey: "rkey1"}, Name: "build"}

	placeCtx, placeCancel := context.WithTimeout(ctx, 10*time.Second)
	defer placeCancel()

	slot, err := mill.place(placeCtx, "dummy", wid, wf)
	if err != nil {
		t.Fatalf("place: %v", err)
	}
	defer slot.Release()

	logPath := models.LogFilePath(millDir, wid)
	ch := bn.Subscribe()
	defer bn.Unsubscribe(ch)

	sawLogContent := make(chan bool, 1)
	go func() {
		ticker := time.NewTicker(20 * time.Millisecond)
		defer ticker.Stop()
		timeout := time.After(10 * time.Second)
		for {
			select {
			case <-ch:
			case <-ticker.C:
			case <-timeout:
				sawLogContent <- false
				return
			}
			data, err := os.ReadFile(logPath)
			if err == nil && strings.Contains(string(data), "echo hi") {
				sawLogContent <- true
				return
			}
		}
	}()

	if err := mill.commitAndWait(placeCtx, wf, nil); err != nil {
		t.Fatalf("commitAndWait: %v, want success", err)
	}

	if !<-sawLogContent {
		t.Fatal("mill live log file never received expected content while job was running")
	}

	if !waitForFileRemoval(t, logPath) {
		t.Fatalf("mill live log file was not removed after terminal artifact was recorded")
	}

	if !waitForStatus(t, bdb, wid, "running") {
		events, _ := bdb.GetEvents(0, 1000)
		t.Logf("mill events after completion: %+v", events)
		t.Fatal("mill never saw streamed running status")
	}
}

func TestExecutorConfiguredLabelsAreStoredOnSession(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	l := slog.New(slog.NewTextHandler(io.Discard, nil))

	millDir := t.TempDir()
	bdb, err := db.Make(ctx, filepath.Join(millDir, "mill.db"))
	if err != nil {
		t.Fatalf("mill db: %v", err)
	}
	bn := notifier.New()
	mill := New(l, Config{LogDir: millDir, ReconnectGrace: time.Minute, BidTimeout: 2 * time.Second})
	mill.Attach(bdb, &bn)
	if err := bdb.AddExecutorToken("exec-labels", HashToken("test-token"), nil, []string{"linux", "arm64", "gpu"}); err != nil {
		t.Fatalf("register executor token: %v", err)
	}

	srv := httptest.NewServer(http.HandlerFunc(mill.HandleExecutorConn))
	defer srv.Close()
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")

	execDir := t.TempDir()
	edb, err := db.Make(ctx, filepath.Join(execDir, "exec.db"))
	if err != nil {
		t.Fatalf("exec db: %v", err)
	}
	en := notifier.New()
	cfg := &config.Config{}
	cfg.Server.Dev = true
	cfg.Server.LogDir = execDir
	cfg.ArtifactStores.Disk.Dir = filepath.Join(execDir, "artifacts")
	cfg.Server.Hostname = "exec-labels"
	cfg.Mill.URL = wsURL
	cfg.Mill.Seats = 2
	cfg.Mill.SharedSecret = "test-token"
	cfg.Mill.Labels = []string{"linux", "arm64", "gpu"}

	engines := map[string]models.Engine{"dummy": dummy.New(l)}
	exec, err := executor.New(cfg, engines, edb, &en, l)
	if err != nil {
		t.Fatalf("executor.New: %v", err)
	}
	go exec.Connect(ctx)

	if !waitForSessionLabels(t, mill, "exec-labels", []string{"linux", "arm64", "gpu"}) {
		t.Fatal("mill session never stored executor labels from hello")
	}
}

func TestEndToEndDummyJobUsesRequiredLabelsAcrossExecutors(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	l := slog.New(slog.NewTextHandler(io.Discard, nil))

	millDir := t.TempDir()
	bdb, err := db.Make(ctx, filepath.Join(millDir, "mill.db"))
	if err != nil {
		t.Fatalf("mill db: %v", err)
	}
	bn := notifier.New()
	mill := New(l, Config{LogDir: millDir, ReconnectGrace: time.Minute, BidTimeout: 2 * time.Second})
	mill.Attach(bdb, &bn)
	if err := bdb.AddExecutorToken("exec-x86", HashToken("token-x86"), nil, []string{"linux/amd64", "kvm"}); err != nil {
		t.Fatalf("register x86 executor token: %v", err)
	}
	if err := bdb.AddExecutorToken("exec-arm", HashToken("token-arm"), nil, []string{"linux/arm64", "kvm"}); err != nil {
		t.Fatalf("register arm executor token: %v", err)
	}

	srv := httptest.NewServer(http.HandlerFunc(mill.HandleExecutorConn))
	defer srv.Close()
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")

	startExecutor := func(name, token string, labels []string) {
		t.Helper()
		execDir := t.TempDir()
		edb, err := db.Make(ctx, filepath.Join(execDir, "exec.db"))
		if err != nil {
			t.Fatalf("%s exec db: %v", name, err)
		}
		en := notifier.New()
		cfg := &config.Config{}
		cfg.Server.Dev = true
		cfg.Server.LogDir = execDir
		cfg.ArtifactStores.Disk.Dir = filepath.Join(execDir, "artifacts")
		cfg.Server.Hostname = name
		cfg.Mill.URL = wsURL
		cfg.Mill.Seats = 1
		cfg.Mill.SharedSecret = token
		cfg.Mill.Labels = labels

		engines := map[string]models.Engine{"dummy": dummy.New(l)}
		exec, err := executor.New(cfg, engines, edb, &en, l)
		if err != nil {
			t.Fatalf("executor.New: %v", err)
		}
		go exec.Connect(ctx)
	}
	startExecutor("exec-x86", "token-x86", []string{"linux/amd64", "kvm"})
	startExecutor("exec-arm", "token-arm", []string{"linux/arm64", "kvm"})

	if !waitForSessionLabels(t, mill, "exec-x86", []string{"linux/amd64", "kvm"}) {
		t.Fatal("x86 executor did not connect with labels")
	}
	if !waitForSessionLabels(t, mill, "exec-arm", []string{"linux/arm64", "kvm"}) {
		t.Fatal("arm executor did not connect with labels")
	}

	be := NewEngine("dummy", mill)
	twf := tangled.Pipeline_Workflow{
		Name:   "build-arm",
		RunsOn: []string{"linux/arm64"},
		Raw:    "steps:\n  - name: hello\n    command: echo hi\n",
	}
	wf, err := be.InitWorkflow(twf, tangled.Pipeline{TriggerMetadata: &tangled.Pipeline_TriggerMetadata{}})
	if err != nil {
		t.Fatalf("InitWorkflow: %v", err)
	}
	wid := models.WorkflowId{PipelineId: models.PipelineId{Knot: "knot.test", Rkey: "rkey-arm"}, Name: "build-arm"}

	placeCtx, placeCancel := context.WithTimeout(ctx, 10*time.Second)
	defer placeCancel()

	slot, err := mill.place(placeCtx, "dummy", wid, wf)
	if err != nil {
		t.Fatalf("place: %v", err)
	}
	defer slot.Release()

	lease := wf.Data.(*millWorkflowState).Lease
	if lease == nil {
		t.Fatal("place did not attach lease to workflow state")
	}
	if lease.nodeID != "exec-arm" {
		t.Fatalf("placed on %q, want exec-arm", lease.nodeID)
	}

	if err := mill.commitAndWait(placeCtx, wf, nil); err != nil {
		t.Fatalf("commitAndWait: %v, want success", err)
	}
}

func waitForSessionLabels(t *testing.T, m *Mill, nodeID string, want []string) bool {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		m.mu.Lock()
		sess := m.sessions[nodeID]
		var got []string
		if sess != nil {
			got = append([]string(nil), sess.labels...)
		}
		m.mu.Unlock()
		if sameStringMultiset(got, want) {
			return true
		}
		time.Sleep(10 * time.Millisecond)
	}
	return false
}

func waitForStatus(t *testing.T, d *db.DB, wid models.WorkflowId, want string) bool {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	aturi := string(wid.PipelineId.AtUri())
	for time.Now().Before(deadline) {
		evs, err := d.GetEvents(0, 1000)
		if err != nil {
			t.Fatalf("GetEvents: %v", err)
		}
		for _, ev := range evs {
			if ev.Nsid != tangled.PipelineStatusNSID {
				continue
			}
			var st tangled.PipelineStatus
			if err := json.Unmarshal(ev.EventJson, &st); err != nil {
				continue
			}
			if st.Pipeline == aturi && st.Workflow == wid.Name && st.Status == want {
				return true
			}
		}
		time.Sleep(50 * time.Millisecond)
	}
	return false
}

func waitForLogFileContent(t *testing.T, path, want string) bool {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		data, err := os.ReadFile(path)
		if err == nil && strings.Contains(string(data), want) {
			return true
		}
		time.Sleep(20 * time.Millisecond)
	}
	return false
}

func waitForFileRemoval(t *testing.T, path string) bool {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		_, err := os.Stat(path)
		if os.IsNotExist(err) {
			return true
		}
		time.Sleep(20 * time.Millisecond)
	}
	return false
}
