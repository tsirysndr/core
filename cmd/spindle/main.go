package main

import (
	"context"
	"log/slog"
	"os"

	"github.com/urfave/cli/v3"
	tlog "tangled.org/core/log"
	"tangled.org/core/spindle"
)

func main() {
	cmd := &cli.Command{
		Name:  "spindle",
		Usage: "spindle continuous integration runner",
		Commands: []*cli.Command{
			Command(),
		},
		DefaultCommand: "run",
	}

	logger := tlog.New("spindle")
	slog.SetDefault(logger)

	ctx := context.Background()
	ctx = tlog.IntoContext(ctx, logger)

	if err := cmd.Run(ctx, os.Args); err != nil {
		logger.Error(err.Error())
		os.Exit(-1)
	}
}

func Command() *cli.Command {
	return &cli.Command{
		Name:  "run",
		Usage: "run the spindle server",
		Action: func(ctx context.Context, cmd *cli.Command) error {
			return spindle.Run(ctx)
		},
	}
}
