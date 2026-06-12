//go:build linux

package main

import (
	"context"
	"log/slog"
	"os"

	tlog "tangled.org/core/log"
)

func main() {
	cmd := SpindleMicroVMRunCommand()

	logger := tlog.New("spindle-microvm-run")
	slog.SetDefault(logger)

	ctx := context.Background()
	ctx = tlog.IntoContext(ctx, logger)

	if err := cmd.Run(ctx, os.Args); err != nil {
		logger.Error(err.Error())
		os.Exit(-1)
	}
}
