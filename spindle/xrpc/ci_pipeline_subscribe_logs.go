package xrpc

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/gorilla/websocket"
	"github.com/hpcloud/tail"
	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/models"
)

func (x *Xrpc) HandleCiSubscribePipelineLogs(w http.ResponseWriter, r *http.Request) {
	var (
		pipelineQuery = r.URL.Query().Get("pipeline")
		workflows     = r.URL.Query()["workflows"]
	)

	pipeline, err := syntax.ParseTID(pipelineQuery)
	if err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("pipeline parameter invalid: %s", pipelineQuery)})
		return
	}

	x.handleSubscribeLogs(w, r, pipeline, workflows)
}

var wsUpgrader = websocket.Upgrader{
	ReadBufferSize:  10_000,
	WriteBufferSize: 10_000,
}

func (x *Xrpc) handleSubscribeLogs(w http.ResponseWriter, r *http.Request, pipeline syntax.TID, workflows []string) {
	l := x.Logger.With("pipeline", pipeline, "workflows", workflows)

	// 1. query the event from database to get the knot
	var eventJson string
	err := x.Db.QueryRow(
		`select event from events where nsid = ? and rkey = ?`,
		tangled.PipelineNSID,
		pipeline.String(),
	).Scan(&eventJson)
	if err != nil {
		l.Error("failed to find pipeline event", "err", err)
		writeJson(w, http.StatusNotFound, atclient.ErrorBody{Name: "NotFound", Message: fmt.Sprintf("pipeline not found: %s", pipeline.String())})
		return
	}

	var tpl tangled.Pipeline
	if err := json.Unmarshal([]byte(eventJson), &tpl); err != nil {
		l.Error("failed to unmarshal pipeline event", "err", err)
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalError", Message: "failed to parse pipeline event"})
		return
	}

	if tpl.TriggerMetadata == nil || tpl.TriggerMetadata.Repo == nil {
		l.Error("pipeline event trigger metadata is incomplete")
		writeJson(w, http.StatusInternalServerError, atclient.ErrorBody{Name: "InternalError", Message: "pipeline event trigger metadata is incomplete"})
		return
	}
	knot := tpl.TriggerMetadata.Repo.Knot

	// 2. if workflows is empty, default to all workflows defined in the pipeline
	if len(workflows) == 0 {
		for _, wf := range tpl.Workflows {
			if wf != nil && wf.Name != "" {
				workflows = append(workflows, wf.Name)
			}
		}
	}

	if len(workflows) == 0 {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "no workflows specified or found"})
		return
	}

	// 3. upgrade to websocket
	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	conn, err := wsUpgrader.Upgrade(w, r, w.Header())
	if err != nil {
		l.Error("websocket upgrade failed", "err", err)
		return
	}
	defer conn.Close()

	lastWriteLk := sync.Mutex{}
	lastWrite := time.Now()

	// Ping loop
	go func() {
		ticker := time.NewTicker(30 * time.Second)
		defer ticker.Stop()

		for {
			select {
			case <-ticker.C:
				lastWriteLk.Lock()
				lw := lastWrite
				lastWriteLk.Unlock()

				if time.Since(lw) < 30*time.Second {
					continue
				}

				if err := conn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(5*time.Second)); err != nil {
					l.Warn("failed to ping client", "err", err)
					cancel()
					return
				}
			case <-ctx.Done():
				return
			}
		}
	}()

	conn.SetPingHandler(func(message string) error {
		err := conn.WriteControl(websocket.PongMessage, []byte(message), time.Now().Add(time.Second*60))
		if err == websocket.ErrCloseSent {
			return nil
		}
		return err
	})

	// Read discard loop
	go func() {
		for {
			_, _, err := conn.ReadMessage()
			if err != nil {
				l.Warn("failed to read message from client", "err", err)
				cancel()
				return
			}
		}
	}()

	eventsChan := make(chan tangled.CiSubscribePipelineLogs_Event, 128)
	wg := sync.WaitGroup{}

	// 4. start a tail reader goroutine for each workflow
	for _, wf := range workflows {
		wg.Add(1)
		go func(wfName string) {
			defer wg.Done()

			wid := models.WorkflowId{
				PipelineId: models.PipelineId{
					Knot: knot,
					Rkey: pipeline.String(),
				},
				Name: wfName,
			}

			// check if finished, but poll database to know when it finishes
			var isFinished bool
			status, err := x.Db.GetStatus(wid)
			if err == nil {
				isFinished = models.StatusKind(status.Status).IsFinish()
			}

			filePath := models.LogFilePath(x.Config.Server.LogDir, wid)

			tailConfig := tail.Config{
				Follow:    !isFinished,
				ReOpen:    !isFinished,
				MustExist: false,
				Location: &tail.SeekInfo{
					Offset: 0,
					Whence: io.SeekStart,
				},
			}

			t, err := tail.TailFile(filePath, tailConfig)
			if err != nil {
				l.Error("failed to tail log file", "workflow", wfName, "err", err)
				return
			}
			defer t.Stop()

			// if we are following, poll status in database to stop tailing when finished
			if !isFinished {
				go func() {
					ticker := time.NewTicker(2 * time.Second)
					defer ticker.Stop()
					for {
						select {
						case <-ctx.Done():
							return
						case <-ticker.C:
							status, err := x.Db.GetStatus(wid)
							if err == nil && models.StatusKind(status.Status).IsFinish() {
								t.Stop()
								return
							}
						}
					}
				}()
			}

			for {
				select {
				case <-ctx.Done():
					return
				case line, ok := <-t.Lines:
					if !ok || line == nil {
						return
					}

					if line.Err != nil {
						l.Warn("error tailing log file", "workflow", wfName, "err", line.Err)
						return
					}

					var logLine models.LogLine
					if err := json.Unmarshal([]byte(line.Text), &logLine); err != nil {
						// if it's not JSON, treat it as a raw data line
						logLine = models.NewDataLogLine(0, line.Text, "stdout")
					}

					var ev tangled.CiSubscribePipelineLogs_Event
					timeStr := logLine.Time.Format(time.RFC3339)
					if logLine.Time.IsZero() {
						timeStr = time.Now().Format(time.RFC3339)
					}

					if logLine.Kind == models.LogKindControl {
						stepKindStr := "user"
						if logLine.StepKind == models.StepKindSystem {
							stepKindStr = "system"
						}
						ev = tangled.CiSubscribePipelineLogs_Event{Control: &tangled.CiSubscribePipelineLogs_Control{
							Time:     timeStr,
							Workflow: wfName,
							Step:     int64(logLine.StepId),
							Content:  logLine.Content,
							Command:  strptrOrNil(logLine.StepCommand),
							Status:   strptrOrNil(string(logLine.StepStatus)),
							Kind:     strptrOrNil(stepKindStr),
						}}
					} else {
						streamType := logLine.Stream
						if streamType != "stdout" && streamType != "stderr" {
							streamType = "stdout"
						}
						ev = tangled.CiSubscribePipelineLogs_Event{Data: &tangled.CiSubscribePipelineLogs_Data{
							Time:     timeStr,
							Workflow: wfName,
							Step:     int64(logLine.StepId),
							Content:  logLine.Content + "\n", // Append newline back since logger trims it
							Stream:   streamType,
						}}
					}

					select {
					case eventsChan <- ev:
					case <-ctx.Done():
						return
					}
				}
			}
		}(wf)
	}

	// Closer goroutine for eventsChan
	go func() {
		wg.Wait()
		close(eventsChan)
	}()

	// Main writer loop
	for {
		select {
		case <-ctx.Done():
			return
		case evt, ok := <-eventsChan:
			if !ok {
				return
			}

			wc, err := conn.NextWriter(websocket.BinaryMessage)
			if err != nil {
				l.Error("failed to get next writer", "err", err)
				return
			}

			err = evt.Serialize(wc)
			if err != nil {
				l.Error("failed to serialize event", "err", err)
				wc.Close()
				return
			}

			if err := wc.Close(); err != nil {
				l.Warn("failed to flush-close event write", "err", err)
				return
			}

			lastWriteLk.Lock()
			lastWrite = time.Now()
			lastWriteLk.Unlock()
		}
	}
}

func strptr(s string) *string { return &s }

func strptrOrNil(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}
