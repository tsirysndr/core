//go:build linux

package spindle

import (
	"context"

	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engines/microvm"
	"tangled.org/core/spindle/models"
)

func newMicrovmEngine(ctx context.Context, cfg *config.Config, d *db.DB) (models.Engine, error) {
	return microvm.New(ctx, cfg, d)
}
