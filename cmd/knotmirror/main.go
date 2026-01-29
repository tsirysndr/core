package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/carlmjohnson/versioninfo"
	"github.com/urfave/cli/v3"
	"tangled.org/core/knotmirror"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/log"
)

func main() {
	if err := run(os.Args); err != nil {
		slog.Error("error running knotmirror", "err", err)
		os.Exit(-1)
	}
}

func run(args []string) error {
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	logger := log.New("knotmirror")
	slog.SetDefault(logger)
	ctx = log.IntoContext(ctx, logger)

	app := cli.Command{
		Name:    "knotmirror",
		Usage:   "knot mirroring service",
		Version: versioninfo.Short(),
	}
	app.Flags = []cli.Flag{}
	app.Commands = []*cli.Command{
		{
			Name:   "serve",
			Usage:  "run the knotmirror daemon",
			Action: runKnotMirror,
			Flags:  []cli.Flag{},
		},
	}
	return app.Run(ctx, args)
}

func runKnotMirror(ctx context.Context, cmd *cli.Command) error {
	logger := log.FromContext(ctx)
	cfg, err := config.Load(ctx)
	if err != nil {
		return err
	}

	logger.Debug("config loaded:", "config", cfg)
	return knotmirror.Run(ctx, cfg)
}
