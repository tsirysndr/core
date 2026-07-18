package executor

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/hpcloud/tail"
	"tangled.org/core/api/tangled"
	millproto "tangled.org/core/spindle/mill/proto"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

func (e *Executor) observeLoop(ctx context.Context, sub <-chan struct{}, cursor int64) {
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-sub:
		case <-ticker.C:
		}
		e.drainEvents(&cursor)
	}
}

func (e *Executor) drainEvents(cursor *int64) {
	events, err := e.db.GetEvents(*cursor, 128)
	if err != nil {
		e.l.Error("drain status events failed", "err", err)
		return
	}
	for _, ev := range events {
		if ev.Created > *cursor {
			*cursor = ev.Created
		}
		st, ok := parseStatus(ev.EventJson)
		if !ok {
			continue
		}
		if err := e.onStatusRow(st); err != nil {
			e.l.Error("process status row failed", "err", err)
		}
	}
}

func (e *Executor) onStatusRow(st *tangled.PipelineStatus) error {
	res := e.reservationFor(st.Pipeline, st.Workflow)
	if res == nil {
		return nil
	}

	if models.StatusKind(st.Status).IsFinish() {
		return e.finishJob(res, st)
	}

	return e.appendStatus(res.leaseID, st)
}

func (e *Executor) finishJob(res *reservation, st *tangled.PipelineStatus) error {
	e.mu.Lock()
	if e.active[res.leaseID] != res {
		e.mu.Unlock()
		return nil
	}
	cancelled := res.cancelled
	e.mu.Unlock()

	// the log tail finalizes first so all log lines precede the terminal event
	if res.stopTail != nil {
		res.stopTail()
	}

	terminalStatus := st.Status
	if cancelled {
		terminalStatus = string(models.StatusKindCancelled)
	}
	var logDir string
	if e.cfg != nil {
		logDir = e.cfg.Server.LogDir
	}
	logPath := models.LogFilePath(logDir, res.wid)

	var errStr string
	if st != nil && st.Error != nil {
		errStr = *st.Error
	}
	var exitCode int64
	if st != nil && st.ExitCode != nil {
		exitCode = *st.ExitCode
	}

	// sha256 of the log file
	hash := ""
	if f, err := os.Open(logPath); err == nil {
		h := sha256.New()
		if _, err := io.Copy(h, f); err == nil {
			hash = "sha256:" + hex.EncodeToString(h.Sum(nil))
		}
		_ = f.Close()
	}

	// refs are opaque keys interpreted by the configured artifact store
	ref := "logs/" + res.leaseID + ".log"

	// persist pending artifact state so a restart can retry the upload
	if e.db != nil {
		_ = e.db.SavePendingArtifact(res.leaseID, res.wid.Name, terminalStatus, errStr, exitCode, ref, hash)
	}

	// upload with a context that survives job cancellation
	cleanupCtx, cancel := context.WithTimeout(context.WithoutCancel(context.Background()), 2*time.Minute)
	defer cancel()

	if e.writer != nil {
		if f, err := os.Open(logPath); err == nil {
			defer f.Close()
			if uploadErr := e.writer.Put(cleanupCtx, ref, f); uploadErr != nil {
				e.l.Error("artifact upload failed", "lease", res.leaseID, "err", uploadErr)
				return fmt.Errorf("artifact upload: %w", uploadErr)
			}
		}
	}

	// append the terminal event with the LogArtifact ref
	if err := e.appendTerminalWithArtifact(res.leaseID, terminalStatus, st, ref, hash); err != nil {
		return err
	}

	// only drop the pending state once the terminal event is in the outbox
	if e.db != nil {
		_ = e.db.RemovePendingArtifact(res.leaseID)
	}
	e.mu.Lock()
	cleanup := e.removeReservationLocked(res, false)
	e.mu.Unlock()

	cleanup()

	e.pushSnapshot()
	return nil
}

func (e *Executor) reservationFor(pipelineAturi, workflow string) *reservation {
	e.mu.Lock()
	defer e.mu.Unlock()
	for _, res := range e.active {
		if string(res.wid.PipelineId.AtUri()) == pipelineAturi && res.wid.Name == workflow {
			return res
		}
	}
	return nil
}

func (e *Executor) maskSecrets(res *reservation, text string) string {
	if res == nil || res.vault == nil {
		return text
	}
	for _, s := range res.vault.secrets {
		if s.Value != "" {
			text = strings.ReplaceAll(text, s.Value, "***")
		}
	}
	return text
}

func (e *Executor) SendLiveLog(leaseID string, raw []byte) error {
	e.connMu.Lock()
	enc := e.enc
	e.connMu.Unlock()
	if enc == nil {
		return nil
	}
	return enc.Encode(&millproto.Message{
		LiveLog: &millv1.LiveLog{
			LeaseId: leaseID,
			RawJson: raw,
		},
	})
}

func (e *Executor) startTail(res *reservation) {
	path := models.LogFilePath(e.cfg.Server.LogDir, res.wid)
	t, err := tail.TailFile(path, tail.Config{
		Follow:    true,
		ReOpen:    true,
		MustExist: false,
		Location:  &tail.SeekInfo{Offset: 0, Whence: io.SeekStart},
		Logger:    tail.DiscardingLogger,
	})
	if err != nil {
		e.l.Error("tail log file failed", "wid", res.wid, "err", err)
		return
	}

	done := make(chan struct{})
	go func() {
		defer close(done)
		for line := range t.Lines {
			if line == nil || line.Err != nil {
				continue
			}
			masked := e.maskSecrets(res, line.Text)
			_ = e.SendLiveLog(res.leaseID, []byte(masked+"\n"))
		}
	}()

	var once sync.Once
	res.stopTail = func() {
		once.Do(func() {
			_ = t.StopAtEOF()
			<-done
		})
	}
}

type memVault struct {
	secrets []secrets.UnlockedSecret
}

func newMemVault(pb []*millv1.Secret) *memVault {
	v := &memVault{secrets: make([]secrets.UnlockedSecret, 0, len(pb))}
	for _, s := range pb {
		v.secrets = append(v.secrets, secrets.UnlockedSecret{
			Key:   s.Key,
			Value: s.Value,
		})
	}
	return v
}

func (v *memVault) GetSecretsUnlocked(ctx context.Context, repo secrets.RepoIdentifier) ([]secrets.UnlockedSecret, error) {
	return v.secrets, nil
}
func (v *memVault) GetSecretsLocked(ctx context.Context, repo secrets.RepoIdentifier) ([]secrets.LockedSecret, error) {
	return nil, nil
}
func (v *memVault) AddSecret(ctx context.Context, s secrets.UnlockedSecret) error { return nil }
func (v *memVault) RemoveSecret(ctx context.Context, s secrets.Secret[any]) error { return nil }

var _ secrets.Manager = (*memVault)(nil)
