package deliberi

import (
	"context"
	"fmt"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/deliberi/config"
	deldb "tangled.org/core/deliberi/db"
	"tangled.org/core/deliberi/mailer"
	delxrpc "tangled.org/core/deliberi/xrpc"
	"tangled.org/core/idresolver"
	"tangled.org/core/log"
	"tangled.org/core/xrpc/serviceauth"
)

func Run(ctx context.Context, cfg *config.Config) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	logger := log.FromContext(ctx)

	database, err := deldb.Make(ctx, cfg.DbPath)
	if err != nil {
		return fmt.Errorf("initializing db: %w", err)
	}

	resolver := idresolver.DefaultResolver(cfg.PlcUrl)

	bobbin := newBobbinClient(cfg.BobbinApiUrl)
	ingester, err := NewIngester(database, bobbin, resolver, cfg.JetstreamEndpoint, cfg.Hostname, log.SubLogger(logger, "ingest"))
	if err != nil {
		return fmt.Errorf("creating ingester: %w", err)
	}
	go func() {
		if err := ingester.Run(ctx); err != nil {
			logger.Error("ingester stopped", "err", err)
			cancel()
		}
	}()

	sender := mailer.New(cfg.Resend, log.SubLogger(logger, "email"))
	dispatcher := NewDispatcher(database, sender, cfg.Resend, cfg.BaseURL, resolver, log.SubLogger(logger, "digest"), cfg.Dev)
	go dispatcher.Start(ctx)

	serviceAuth := serviceauth.NewServiceAuth(logger, resolver.Directory(), serviceauth.DidWeb(cfg.Hostname).String())
	x := &delxrpc.Xrpc{
		DB:          database,
		Config:      cfg,
		Logger:      log.SubLogger(logger, "xrpc"),
		ServiceAuth: serviceAuth,
		IdResolver:  resolver,
		Sender:      sender,
	}

	srv := &http.Server{Addr: cfg.ListenAddr, Handler: chiMount(x)}
	go func() {
		logger.Info("starting http server", "addr", cfg.ListenAddr)
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			logger.Error("http server failed", "err", err)
			cancel()
		}
	}()

	logger.Info("startup complete")
	<-ctx.Done()
	logger.Info("received shutdown signal", "reason", ctx.Err())

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutdownCancel()
	if err := srv.Shutdown(shutdownCtx); err != nil {
		logger.Error("http shutdown", "err", err)
	}
	if err := database.Close(); err != nil {
		logger.Error("db close", "err", err)
	}
	logger.Info("shutdown complete")
	return nil
}

func chiMount(x *delxrpc.Xrpc) http.Handler {
	mux := chi.NewRouter()
	mux.Mount("/xrpc", x.Router())
	return mux
}
