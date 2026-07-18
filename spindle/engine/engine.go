package engine

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"sync"
	"time"

	"tangled.org/core/notifier"
	"tangled.org/core/spindle/artifactstore"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

var (
	ErrTimedOut         = errors.New("timed out")
	ErrWorkflowFailed   = errors.New("workflow failed")
	ErrWorkflowCanceled = errors.New("workflow canceled")
)

var (
	activeMu      sync.Mutex
	activeCancels = make(map[models.WorkflowId]context.CancelCauseFunc)
)

func CancelWorkflow(wid models.WorkflowId) {
	activeMu.Lock()
	cancel, ok := activeCancels[wid]
	activeMu.Unlock()
	if ok {
		cancel(ErrWorkflowCanceled)
	}
}

// user cancel, timeout is DeadlineExceeded
func isCanceled(wfCtx context.Context) bool {
	return errors.Is(context.Cause(wfCtx), ErrWorkflowCanceled)
}

// for when recording early wf cancellations
func writeWfError(db *db.DB, n *notifier.Notifier, l *slog.Logger, wfCtx context.Context, wid models.WorkflowId, phase string, err error) {
	l = l.With("wid", wid, "phase", phase)
	switch {
	case isCanceled(wfCtx):
		l.Info("workflow canceled")
		if dbErr := db.StatusCancelled(wid, "User canceled the workflow", -1, n); dbErr != nil {
			l.Error("failed to set workflow status to cancelled", "err", dbErr)
		}
	case errors.Is(err, ErrTimedOut) || errors.Is(wfCtx.Err(), context.DeadlineExceeded):
		l.Info("workflow timed out")
		if dbErr := db.StatusTimeout(wid, n); dbErr != nil {
			l.Error("failed to set workflow status to timeout", "err", dbErr)
		}
	default:
		l.Error("workflow failed", "err", err)
		if dbErr := db.StatusFailed(wid, err.Error(), -1, n); dbErr != nil {
			l.Error("failed to set workflow status to failed", "err", dbErr)
		}
	}
}

// the mill streams the executor's real log file into place itself
// a local logger here would only write competing lines
type workflowLoggerProvider interface {
	WorkflowLogger(wid models.WorkflowId) models.WorkflowLogger
}

// for engines that manage status updates outside StartWorkflows
type RemoteStatusEngine interface {
	AuthorsRemoteStatus()
}

func reportWorkflowStatusError(l *slog.Logger, database *db.DB, n *notifier.Notifier, wid models.WorkflowId, err error) {
	if errors.Is(err, ErrTimedOut) {
		dbErr := database.StatusTimeout(wid, n)
		if dbErr != nil {
			l.Error("failed to set workflow status to timeout", "wid", wid, "err", dbErr)
		}
	} else if errors.Is(err, ErrWorkflowCanceled) {
		dbErr := database.StatusCancelled(wid, err.Error(), -1, n)
		if dbErr != nil {
			l.Error("failed to set workflow status to cancelled", "wid", wid, "err", dbErr)
		}
	} else {
		dbErr := database.StatusFailed(wid, err.Error(), -1, n)
		if dbErr != nil {
			l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
		}
	}
}

func StartWorkflows(l *slog.Logger, vault secrets.Manager, cfg *config.Config, stores *artifactstore.Stores, db *db.DB, n *notifier.Notifier, ctx context.Context, pipeline *models.Pipeline, pipelineId models.PipelineId) {
	l.Info("starting all workflows in parallel", "pipeline", pipelineId)

	var allSecrets []secrets.UnlockedSecret
	// never pass secrets to pipelines that run untrusted (e.g. fork) code
	if pipeline.TrustedSource && pipeline.RepoDid != "" {
		if res, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(pipeline.RepoDid.String())); err == nil {
			allSecrets = res
		}
	} else if !pipeline.TrustedSource {
		l.Info("skipping secrets for untrusted pipeline source", "pipeline", pipelineId)
	}

	secretValues := make([]string, len(allSecrets))
	for i, s := range allSecrets {
		secretValues[i] = s.Value
	}

	// wid.String() is lossy so two different names can map to the same key
	// eg. "foo bar" and "foo-bar"...
	wfCounts := make(map[string]int)
	for _, wfs := range pipeline.Workflows {
		for _, w := range wfs {
			wid := models.WorkflowId{
				PipelineId: pipelineId,
				Name:       w.Name,
			}
			wfCounts[wid.String()]++
		}
	}
	var wg sync.WaitGroup
	for eng, wfs := range pipeline.Workflows {
		workflowTimeout := eng.WorkflowTimeout()
		l.Info("using workflow timeout", "timeout", workflowTimeout)

		for _, w := range wfs {
			w := w
			wid := models.WorkflowId{
				PipelineId: pipelineId,
				Name:       w.Name,
			}

			if wfCounts[wid.String()] > 1 {
				l.Warn("skipping workflow due to name collision", "wid", wid, "key", wid.String())
				dbErr := db.StatusFailed(wid, fmt.Sprintf("colliding workflow name: %s; rename to something else", wid.String()), -1, n)
				if dbErr != nil {
					l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
				}
				continue
			}

			wg.Go(func() {
				if st, err := db.GetStatus(wid); err == nil && models.StatusKind(st.Status).IsFinish() {
					l.Info("skipping finished workflow", "wid", wid, "status", st.Status)
					return
				}
				var err error
				var wfLogger models.WorkflowLogger
				if p, ok := eng.(workflowLoggerProvider); ok {
					wfLogger = p.WorkflowLogger(wid)
				} else if fileLogger, err := models.NewFileWorkflowLogger(cfg.Server.LogDir, wid, secretValues); err != nil {
					l.Warn("failed to setup step logger; logs will not be persisted", "error", err)
					wfLogger = models.NullLogger{}
				} else {
					l.Info("setup step logger; logs will be persisted", "logDir", cfg.Server.LogDir, "wid", wid)
					wfLogger = fileLogger
					defer archiveWorkflowLog(l, stores, db, cfg.Server.LogDir, wid)
					defer fileLogger.Close()
				}

				timeoutCtx, timeoutCancel := context.WithTimeout(ctx, workflowTimeout)
				defer timeoutCancel()

				wfCtx, userCancel := context.WithCancelCause(timeoutCtx)
				defer userCancel(nil)

				// allow wf context to be cancelled properly by manual cancel
				activeMu.Lock()
				activeCancels[wid] = userCancel
				activeMu.Unlock()
				defer func() {
					activeMu.Lock()
					delete(activeCancels, wid)
					activeMu.Unlock()
				}()

				l.Info("waiting for slot", "wid", wid)
				slot := WorkflowSlot(NoopSlot{})
				_, remoteStatus := eng.(RemoteStatusEngine)

				if s, ok := eng.(WorkflowSlotter); ok {
					slot, err = s.AcquireWorkflowSlot(wfCtx, wid, &w, Wait)
					if err != nil {
						writeWfError(db, n, l, wfCtx, wid, "waiting for slot", err)
						return
					}
				}
				defer slot.Release()

				if !remoteStatus {
					err := db.StatusRunning(wid, n)
					if err != nil {
						l.Error("failed to set workflow status to running", "wid", wid, "err", err)
						return
					}
				}

				err = eng.SetupWorkflow(wfCtx, wid, &w, wfLogger)
				if err != nil {
					if !isCanceled(wfCtx) {
						if destroyErr := eng.DestroyWorkflow(ctx, wid); destroyErr != nil {
							l.Error("failed to destroy workflow after setup failure", "error", destroyErr)
						}
					}
					if !remoteStatus {
						writeWfError(db, n, l, wfCtx, wid, "setting up workflow", err)
					}
					return
				}
				defer eng.DestroyWorkflow(ctx, wid)

				for stepIdx, step := range w.Steps {
					if wfLogger != nil {
						wfLogger.
							ControlWriter(stepIdx, step, models.StepStatusStart).
							Write([]byte{0})
					}

					err = eng.RunStep(wfCtx, wid, &w, stepIdx, allSecrets, wfLogger)

					if wfLogger != nil {
						wfLogger.
							ControlWriter(stepIdx, step, models.StepStatusEnd).
							Write([]byte{0})
					}

					if err != nil {
						if !remoteStatus {
							writeWfError(db, n, l, wfCtx, wid, "running step", err)
						}
						return
					}
				}

				if isCanceled(wfCtx) {
					if !remoteStatus {
						writeWfError(db, n, l, wfCtx, wid, "before success", nil)
					}
					return
				}

				if !remoteStatus {
					err = db.StatusSuccess(wid, n)
					if err != nil {
						l.Error("failed to set workflow status to success", "wid", wid, "err", err)
					}
				}
			})
		}
	}

	wg.Wait()
	l.Info("all workflows completed")
}

func archiveWorkflowLog(l *slog.Logger, stores *artifactstore.Stores, database *db.DB, logDir string, wid models.WorkflowId) {
	if stores == nil {
		return
	}
	logPath := models.LogFilePath(logDir, wid)
	file, err := os.Open(logPath)
	if err != nil {
		l.Error("open workflow log for archival", "wid", wid, "err", err)
		return
	}
	hash := sha256.New()
	if _, err := io.Copy(hash, file); err != nil {
		_ = file.Close()
		l.Error("hash workflow log", "wid", wid, "err", err)
		return
	}
	_ = file.Close()

	ref := wid.String() + ".log"
	uploadCtx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	errs := stores.PutFile(uploadCtx, ref, logPath)
	for _, err := range errs {
		l.Error("archive workflow log", "wid", wid, "err", err)
	}
	if len(errs) == len(stores.Names()) {
		return
	}
	digest := "sha256:" + hex.EncodeToString(hash.Sum(nil))
	if err := database.SaveArtifactRef(wid.String(), wid.Name, ref, digest); err != nil {
		l.Error("save workflow log artifact", "wid", wid, "err", err)
	}
}
