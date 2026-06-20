package spindle

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"

	"tangled.org/core/eventstream"
	"tangled.org/core/log"
	"tangled.org/core/spindle/models"

	"github.com/go-chi/chi/v5"
	"github.com/gorilla/websocket"
	"github.com/hpcloud/tail"
)

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

func (s *Spindle) Events(w http.ResponseWriter, r *http.Request) {
	l := log.SubLogger(s.l, "eventstream")
	l.Debug("received new connection")

	err := eventstream.Stream(w, r, eventstream.StreamConfig{
		Backend:  s.db,
		Notifier: s.n,
		Logger:   l,
	})
	if err != nil && !errors.Is(err, eventstream.ErrDrainCap) {
		l.Error("event stream ended with error", "err", err)
	}
}

func (s *Spindle) Logs(w http.ResponseWriter, r *http.Request) {
	wid, err := getWorkflowID(r)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	l := s.l.With("handler", "Logs")
	l = s.l.With("wid", wid)

	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		l.Error("websocket upgrade failed", "err", err)
		http.Error(w, "failed to upgrade", http.StatusInternalServerError)
		return
	}
	defer func() {
		_ = conn.WriteControl(
			websocket.CloseMessage,
			websocket.FormatCloseMessage(websocket.CloseNormalClosure, "log stream complete"),
			time.Now().Add(time.Second),
		)
		conn.Close()
	}()
	l.Debug("upgraded http to wss")

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	go func() {
		for {
			if _, _, err := conn.NextReader(); err != nil {
				l.Debug("client disconnected", "err", err)
				cancel()
				return
			}
		}
	}()

	if err := s.streamLogsFromDisk(ctx, conn, wid); err != nil {
		l.Info("log stream ended", "err", err)
	}

	l.Info("logs connection closed")
}

func (s *Spindle) streamLogsFromDisk(ctx context.Context, conn *websocket.Conn, wid models.WorkflowId) error {
	status, err := s.db.GetStatus(wid)
	if err != nil {
		return err
	}
	isFinished := models.StatusKind(status.Status).IsFinish()

	filePath := models.LogFilePath(s.cfg.Server.LogDir, wid)



	config := tail.Config{
		Follow:    !isFinished,
		ReOpen:    !isFinished,
		MustExist: false,
		Location: &tail.SeekInfo{
			Offset: 0,
			Whence: io.SeekStart,
		},
		// Logger: tail.DiscardingLogger,
	}

	t, err := tail.TailFile(filePath, config)
	if err != nil {
		return fmt.Errorf("failed to tail log file: %w", err)
	}
	defer t.Stop()

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case line := <-t.Lines:
			if line == nil && isFinished {
				return fmt.Errorf("tail completed")
			}

			if line == nil {
				return fmt.Errorf("tail channel closed unexpectedly")
			}

			if line.Err != nil {
				return fmt.Errorf("error tailing log file: %w", line.Err)
			}

			if err := conn.WriteMessage(websocket.TextMessage, []byte(line.Text)); err != nil {
				return fmt.Errorf("failed to write to websocket: %w", err)
			}
		case <-time.After(30 * time.Second):
			// send a keep-alive
			if err := conn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(time.Second)); err != nil {
				return fmt.Errorf("failed to write control: %w", err)
			}
		}
	}
}

func getWorkflowID(r *http.Request) (models.WorkflowId, error) {
	knot := chi.URLParam(r, "knot")
	rkey := chi.URLParam(r, "rkey")
	name := chi.URLParam(r, "name")

	if knot == "" || rkey == "" || name == "" {
		return models.WorkflowId{}, fmt.Errorf("missing required parameters")
	}

	return models.WorkflowId{
		PipelineId: models.PipelineId{
			Knot: knot,
			Rkey: rkey,
		},
		Name: name,
	}, nil
}
