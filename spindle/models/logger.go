package models

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

type WorkflowLogger interface {
	Close() error
	DataWriter(idx int, stream string) io.Writer
	ControlWriter(idx int, step Step, stepStatus StepStatus) io.Writer
}

type NullLogger struct{}

func (l NullLogger) Close() error                                { return nil }
func (l NullLogger) DataWriter(idx int, stream string) io.Writer { return io.Discard }
func (l NullLogger) ControlWriter(idx int, step Step, stepStatus StepStatus) io.Writer {
	return io.Discard
}

type FileWorkflowLogger struct {
	file        *os.File
	encoder     *json.Encoder
	mask        *SecretMask
	dataWriters []*dataWriter
}

func NewFileWorkflowLogger(baseDir string, wid WorkflowId, secretValues []string) (WorkflowLogger, error) {
	path := LogFilePath(baseDir, wid)
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		return nil, fmt.Errorf("creating log file: %w", err)
	}
	return &FileWorkflowLogger{
		file:    file,
		encoder: json.NewEncoder(file),
		mask:    NewSecretMask(secretValues),
	}, nil
}

func LogFilePath(baseDir string, workflowID WorkflowId) string {
	logFilePath := filepath.Join(baseDir, fmt.Sprintf("%s.log", workflowID.String()))
	return logFilePath
}

func (l *FileWorkflowLogger) Close() error {
	for _, w := range l.dataWriters {
		if err := w.flush(); err != nil {
			return err
		}
	}
	return l.file.Close()
}

func (l *FileWorkflowLogger) DataWriter(idx int, stream string) io.Writer {
	w := &dataWriter{
		logger: l,
		idx:    idx,
		stream: stream,
	}
	l.dataWriters = append(l.dataWriters, w)
	return w
}

func (l *FileWorkflowLogger) ControlWriter(idx int, step Step, stepStatus StepStatus) io.Writer {
	return &controlWriter{
		logger:     l,
		idx:        idx,
		step:       step,
		stepStatus: stepStatus,
	}
}

type dataWriter struct {
	logger *FileWorkflowLogger
	idx    int
	stream string
	// trailing bytes held back so a secret split across writes still
	// matches, flushed on Close or once enough data arrives
	pending []byte
}

func (w *dataWriter) Write(p []byte) (int, error) {
	w.pending = append(w.pending, p...)
	if err := w.flushCompleteLines(); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (w *dataWriter) flushCompleteLines() error {
	limit := len(w.pending) - w.logger.mask.Window()
	if limit <= 0 {
		return nil
	}

	for {
		lineEnd := bytes.IndexByte(w.pending[:limit], '\n')
		if lineEnd < 0 {
			return nil
		}
		lineEnd++
		line := append([]byte(nil), w.pending[:lineEnd]...)
		w.pending = w.pending[lineEnd:]
		limit -= lineEnd
		if err := w.emit(line); err != nil {
			return err
		}
	}
}

// the writer is done, so a buffered tail can no longer grow into a full
// secret and goes out as-is
func (w *dataWriter) flush() error {
	if len(w.pending) == 0 {
		return nil
	}
	pending := w.pending
	w.pending = nil
	return w.emit(pending)
}

func (w *dataWriter) emit(p []byte) error {
	line := strings.TrimRight(string(p), "\r\n")
	if w.logger.mask != nil {
		line = w.logger.mask.Mask(line)
	}
	entry := NewDataLogLine(w.idx, line, w.stream)
	return w.logger.encoder.Encode(entry)
}

type controlWriter struct {
	logger     *FileWorkflowLogger
	idx        int
	step       Step
	stepStatus StepStatus
}

func (w *controlWriter) Write(_ []byte) (int, error) {
	entry := NewControlLogLine(w.idx, w.step, w.stepStatus)
	if err := w.logger.encoder.Encode(entry); err != nil {
		return 0, err
	}
	return len(w.step.Name()), nil
}
