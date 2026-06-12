package engine

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"path/filepath"
	"sync"

	"tangled.org/core/notifier"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

var (
	ErrTimedOut       = errors.New("timed out")
	ErrWorkflowFailed = errors.New("workflow failed")
)

type workflowFinalizer interface {
	FinalizeWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) error
}

func StartWorkflows(l *slog.Logger, vault secrets.Manager, cfg *config.Config, db *db.DB, n *notifier.Notifier, ctx context.Context, pipeline *models.Pipeline, pipelineId models.PipelineId) {
	l.Info("starting all workflows in parallel", "pipeline", pipelineId)

	// extract secrets
	var allSecrets []secrets.UnlockedSecret
	if pipeline.RepoDid != "" {
		if res, err := vault.GetSecretsUnlocked(ctx, secrets.RepoIdentifier(pipeline.RepoDid.String())); err == nil {
			allSecrets = res
		}
	}

	secretValues := make([]string, len(allSecrets))
	for i, s := range allSecrets {
		secretValues[i] = s.Value
	}

	s3, err := NewS3(cfg.S3.LogBucket)
	if err != nil {
		l.Error("error creating s3 client", "err", err)
	}

	var wg sync.WaitGroup
	for eng, wfs := range pipeline.Workflows {
		workflowTimeout := eng.WorkflowTimeout()
		l.Info("using workflow timeout", "timeout", workflowTimeout)

		for _, w := range wfs {
			wg.Add(1)
			go func() {
				defer wg.Done()

				wid := models.WorkflowId{
					PipelineId: pipelineId,
					Name:       w.Name,
				}

				defer func() {
					if s3 != nil {
						logFile := filepath.Join(cfg.Server.LogDir, fmt.Sprintf("%s.log", wid.String()))
						if err := s3.WriteFile(ctx, logFile); err != nil {
							l.Error("error uploading logs", "err", err)
						}
					}
				}()

				wfLogger, err := models.NewFileWorkflowLogger(cfg.Server.LogDir, wid, secretValues)
				if err != nil {
					l.Warn("failed to setup step logger; logs will not be persisted", "error", err)
					wfLogger = models.NullLogger{}
				} else {
					l.Info("setup step logger; logs will be persisted", "logDir", cfg.Server.LogDir, "wid", wid)
					defer wfLogger.Close()
				}

				l.Info("waiting for slot", "wid", wid)
				slot := WorkflowSlot(NoopSlot{})
				if s, ok := eng.(WorkflowSlotter); ok {
					var err error
					slot, err = s.AcquireWorkflowSlot(ctx, wid, &w)
					if err != nil {
						l.Error("failed to acquire slot", "wid", wid, "err", err)
						dbErr := db.StatusFailed(wid, err.Error(), -1, n)
						if dbErr != nil {
							l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
						}
						return
					}
				}
				defer slot.Release()

				err = db.StatusRunning(wid, n)
				if err != nil {
					l.Error("failed to set workflow status to running", "wid", wid, "err", err)
					return
				}

				err = eng.SetupWorkflow(ctx, wid, &w, wfLogger)
				if err != nil {
					// TODO(winter): Should this always set StatusFailed?
					// In the original, we only do in a subset of cases.
					l.Error("setting up workflow", "wid", wid, "err", err)

					destroyErr := eng.DestroyWorkflow(ctx, wid)
					if destroyErr != nil {
						l.Error("failed to destroy workflow after setup failure", "error", destroyErr)
					}

					dbErr := db.StatusFailed(wid, err.Error(), -1, n)
					if dbErr != nil {
						l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
					}
					return
				}
				defer eng.DestroyWorkflow(ctx, wid)

				ctx, cancel := context.WithTimeout(ctx, workflowTimeout)
				defer cancel()

				for stepIdx, step := range w.Steps {
					// log start of step
					if wfLogger != nil {
						wfLogger.
							ControlWriter(stepIdx, step, models.StepStatusStart).
							Write([]byte{0})
					}

					err = eng.RunStep(ctx, wid, &w, stepIdx, allSecrets, wfLogger)

					// log end of step
					if wfLogger != nil {
						wfLogger.
							ControlWriter(stepIdx, step, models.StepStatusEnd).
							Write([]byte{0})
					}

					if err != nil {
						if errors.Is(err, ErrTimedOut) {
							dbErr := db.StatusTimeout(wid, n)
							if dbErr != nil {
								l.Error("failed to set workflow status to timeout", "wid", wid, "err", dbErr)
							}
						} else {
							dbErr := db.StatusFailed(wid, err.Error(), -1, n)
							if dbErr != nil {
								l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
							}
						}
						return
					}
				}

				if finalizer, ok := eng.(workflowFinalizer); ok {
					if err := finalizer.FinalizeWorkflow(ctx, wid, &w, wfLogger); err != nil {
						dbErr := db.StatusFailed(wid, err.Error(), -1, n)
						if dbErr != nil {
							l.Error("failed to set workflow status to failed", "wid", wid, "err", dbErr)
						}
						return
					}
				}

				err = db.StatusSuccess(wid, n)
				if err != nil {
					l.Error("failed to set workflow status to success", "wid", wid, "err", err)
				}
			}()
		}
	}

	wg.Wait()
	l.Info("all workflows completed")
}
