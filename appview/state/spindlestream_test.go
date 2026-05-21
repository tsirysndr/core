package state

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/pipelines"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
	"tangled.org/core/notifier"
	spindledb "tangled.org/core/spindle/db"
	spindlemodels "tangled.org/core/spindle/models"
)

func TestColdStart_SpindleEventsRebuildPipelineStatuses(t *testing.T) {
	ctx := t.Context()

	spindleDB, err := spindledb.Make(ctx, filepath.Join(t.TempDir(), "spindle.db"))
	if err != nil {
		t.Fatalf("spindle Make: %v", err)
	}
	t.Cleanup(func() { spindleDB.Close() })

	n := notifier.New()
	workflowId := spindlemodels.WorkflowId{
		PipelineId: spindlemodels.PipelineId{Knot: "knot.boltless.example", Rkey: "pipeline-rk1"},
		Name:       "build",
	}
	for _, step := range []func() error{
		func() error { return spindleDB.StatusPending(workflowId, &n) },
		func() error { return spindleDB.StatusRunning(workflowId, &n) },
		func() error { return spindleDB.StatusSuccess(workflowId, &n) },
	} {
		if err := step(); err != nil {
			t.Fatalf("seed spindle event: %v", err)
		}
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/events", func(w http.ResponseWriter, r *http.Request) {
		_ = eventstream.Stream(w, r, eventstream.StreamConfig{
			Backend:  spindleDB,
			Notifier: &n,
			Logger:   slog.New(slog.NewTextHandler(io.Discard, nil)),
		})
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	source := ec.Source{Kind: "test", Host: strings.TrimPrefix(srv.URL, "http://")}

	appviewDB, err := db.Make(ctx, filepath.Join(t.TempDir(), "appview.db"))
	if err != nil {
		t.Fatalf("appview Make: %v", err)
	}
	t.Cleanup(func() { appviewDB.Close() })

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	processFunc := spindleIngester(appviewDB, pipelines.NewStatusNotifier())

	cfg := ec.ConsumerConfig{
		ProcessFunc:       processFunc,
		WorkerCount:       1,
		QueueSize:         16,
		ConnectionTimeout: 2 * time.Second,
		CursorStore:       &cursor.MemoryStore{},
		URLFunc:           ec.DefaultURL(true),
		Logger:            logger,
	}
	c := ec.NewConsumer(cfg)

	consumerCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	c.Start(consumerCtx)
	c.AddSource(consumerCtx, source)

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		var n int
		if err := appviewDB.QueryRow(`select count(*) from pipeline_statuses`).Scan(&n); err != nil {
			t.Fatalf("count: %v", err)
		}
		if n >= 3 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	rows, err := appviewDB.Query(`
		select spindle, pipeline_knot, pipeline_rkey, workflow, status
		from pipeline_statuses
		order by created asc
	`)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	defer rows.Close()

	type rec struct {
		spindle, knot, rkey, workflow, status string
	}
	var got []rec
	for rows.Next() {
		var r rec
		if err := rows.Scan(&r.spindle, &r.knot, &r.rkey, &r.workflow, &r.status); err != nil {
			t.Fatalf("scan: %v", err)
		}
		got = append(got, r)
	}

	if len(got) != 3 {
		t.Fatalf("pipeline_statuses rows = %d, want 3: %+v", len(got), got)
	}

	wantStatuses := []string{"pending", "running", "success"}
	gotStatuses := map[string]bool{}
	for _, r := range got {
		gotStatuses[r.status] = true
		if r.spindle != source.Host {
			t.Errorf("spindle = %q, want %q", r.spindle, source.Host)
		}
		if r.knot != workflowId.Knot {
			t.Errorf("pipeline_knot = %q, want %q", r.knot, workflowId.Knot)
		}
		if r.rkey != workflowId.Rkey {
			t.Errorf("pipeline_rkey = %q, want %q", r.rkey, workflowId.Rkey)
		}
		if r.workflow != workflowId.Name {
			t.Errorf("workflow = %q, want %q", r.workflow, workflowId.Name)
		}
	}
	for _, want := range wantStatuses {
		if !gotStatuses[want] {
			t.Errorf("missing status %q in projection", want)
		}
	}
}
