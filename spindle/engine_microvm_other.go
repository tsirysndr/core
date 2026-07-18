//go:build !linux

package spindle

import (
	"context"
	"fmt"

	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
)

func newMicrovmEngine(context.Context, *config.Config, *db.DB) (models.Engine, error) {
	return nil, fmt.Errorf("microvm engine is only supported on Linux")
}
