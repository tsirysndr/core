package main

import (
	"context"
	"net/http"
	"os"

	"github.com/prometheus/client_golang/prometheus/promhttp"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/state"
	tlog "tangled.org/core/log"
)

func main() {
	ctx := context.Background()
	logger := tlog.New("appview")
	ctx = tlog.IntoContext(ctx, logger)

	c, err := config.LoadConfig(ctx)
	if err != nil {
		logger.Error("failed to load config", "error", err)
		return
	}

	state, err := state.Make(ctx, c)
	defer func() {
		if err := state.Close(); err != nil {
			logger.Error("failed to close state", "err", err)
		}
	}()

	if err != nil {
		logger.Error("failed to start appview", "err", err)
		os.Exit(-1)
	}

	logger.Info("starting server", "address", c.Core.ListenAddr)

	go func() {
		logger.Info("starting metrics server", "address", c.Core.MetricsListenAddr)
		http.Handle("/metrics", promhttp.Handler())
		if err := http.ListenAndServe(c.Core.MetricsListenAddr, nil); err != nil {
			logger.Error("failed to start metrics server", "err", err)
		}
	}()

	if c.SSH.Enabled {
		sshServer := state.NewSSHServer()
		go func() {
			if err := sshServer.ListenAndServe(ctx); err != nil {
				logger.Error("SSH server stopped", "err", err)
			}
		}()
	}

	if err := http.ListenAndServe(c.Core.ListenAddr, state.Router()); err != nil {
		logger.Error("failed to start appview", "err", err)
	}
}
