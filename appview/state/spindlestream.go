package state

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pipelines"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventstream"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	spindle "tangled.org/core/spindle/models"
)

func Spindlestream(ctx context.Context, c *config.Config, d *db.DB, enforcer *rbac.Enforcer, pn *pipelines.StatusNotifier) (*ec.Consumer, error) {
	spindles, err := db.GetSpindles(ctx, d, orm.FilterIsNot("verified", "null"))
	if err != nil {
		return nil, err
	}

	hosts := make([]string, len(spindles))
	for i, s := range spindles {
		hosts[i] = s.Instance
	}

	return bootstrapStream(
		ctx, "spindlestream", ec.KindSpindle, hosts, c.Redis.Addr,
		c.Spindlestream,
		spindleIngester(d, pn),
	), nil
}

func spindleIngester(d *db.DB, pn *pipelines.StatusNotifier) ec.ProcessFunc {
	return func(ctx context.Context, source ec.Source, msg eventstream.Event) error {
		switch msg.Nsid {
		case tangled.PipelineStatusNSID:
			return ingestPipelineStatus(ctx, d, pn, source, msg)
		}
		return nil
	}
}

func ingestPipelineStatus(ctx context.Context, d *db.DB, pn *pipelines.StatusNotifier, source ec.Source, msg eventstream.Event) error {
	var record tangled.PipelineStatus
	err := json.Unmarshal(msg.EventJson, &record)
	if err != nil {
		return err
	}

	pipelineUri, err := syntax.ParseATURI(record.Pipeline)
	if err != nil {
		return err
	}

	exitCode := 0
	if record.ExitCode != nil {
		exitCode = int(*record.ExitCode)
	}

	// pick the record creation time if possible, or use time.Now
	created := time.Now()
	if t, err := time.Parse(time.RFC3339, record.CreatedAt); err == nil && created.After(t) {
		created = t
	}

	status := models.PipelineStatus{
		Spindle:      source.Host,
		Rkey:         msg.Rkey,
		PipelineKnot: strings.TrimPrefix(pipelineUri.Authority().String(), "did:web:"),
		PipelineRkey: pipelineUri.RecordKey().String(),
		Created:      created,
		Workflow:     record.Workflow,
		Status:       spindle.StatusKind(record.Status),
		Error:        record.Error,
		ExitCode:     exitCode,
	}

	err = db.AddPipelineStatus(ctx, d, status)
	if err != nil {
		return fmt.Errorf("failed to add pipeline status: %w", err)
	}

	pn.Publish(pipelineUri)

	return nil
}
