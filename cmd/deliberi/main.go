package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/carlmjohnson/versioninfo"
	"github.com/urfave/cli/v3"
	"tangled.org/core/deliberi"
	"tangled.org/core/deliberi/config"
	"tangled.org/core/log"
)

func main() {
	if err := run(os.Args); err != nil {
		slog.Error("error running deliberi", "err", err)
		os.Exit(-1)
	}
}

func run(args []string) error {
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	logger := log.New("deliberi")
	slog.SetDefault(logger)
	ctx = log.IntoContext(ctx, logger)

	app := cli.Command{
		Name:    "deliberi",
		Usage:   "tngl.sh notification + email service",
		Version: versioninfo.Short(),
	}
	app.Commands = []*cli.Command{
		{
			Name:   "serve",
			Usage:  "run the deliberi daemon",
			Action: runDeliberi,
		},
	}
	return app.Run(ctx, args)
}

func runDeliberi(ctx context.Context, cmd *cli.Command) error {
	logger := log.FromContext(ctx)
	cfg, err := config.Load(ctx)
	if err != nil {
		return err
	}
	logger.Debug("config loaded", "config", cfg)
	return deliberi.Run(ctx, cfg)
}
