package knotmirror

import (
	"context"
	"fmt"
	"net/http"
	_ "net/http/pprof"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/knotstream"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/knotmirror/xrpc"
	"tangled.org/core/log"
)

func Run(ctx context.Context, cfg *config.Config) error {
	// make sure every services are cleaned up on fast return
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	logger := log.FromContext(ctx)

	db, err := db.Make(ctx, cfg.DbUrl, 32)
	if err != nil {
		return fmt.Errorf("initializing db: %w", err)
	}

	resolver := idresolver.DefaultResolver(cfg.PlcUrl)

	// NOTE: using plain git-cli for clone/fetch as go-git is too memory-intensive.
	gitm := NewCliGitMirrorManager(cfg.GitRepoBasePath, cfg.KnotUseSSL)

	res, err := db.ExecContext(ctx,
		`update repos set state = $1 where state = $2`,
		models.RepoStateDesynchronized,
		models.RepoStateResyncing,
	)
	if err != nil {
		return fmt.Errorf("clearing resyning states: %w", err)
	}
	rows, err := res.RowsAffected()
	if err != nil {
		return fmt.Errorf("getting affected rows: %w", err)
	}
	logger.Info(fmt.Sprintf("clearing resyning states: %d records updated", rows))

	xrpc := xrpc.New(logger, cfg, db, resolver)
	knotstream := knotstream.NewKnotStream(logger, db, cfg)
	crawler := NewCrawler(logger, db)
	resyncer := NewResyncer(logger, db, gitm, cfg)
	adminpage := NewAdminServer(logger, db, resyncer)

	// maintain repository list with tap
	// NOTE: this can be removed once we introduce did-for-repo because then we can just listen to KnotStream for #identity events.
	tap := NewTapClient(logger, cfg, db, gitm, knotstream)

	// start http server
	go func() {
		logger.Info("starting http server", "addr", cfg.Listen)

		mux := chi.NewRouter()
		mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
			w.Write([]byte("Welcome to a knotmirror server.\n"))
		})
		mux.Mount("/xrpc", xrpc.Router())

		if err := http.ListenAndServe(cfg.Listen, mux); err != nil {
			logger.Error("xrpc server failed", "error", err)
		}
	}()

	// start metrics endpoint
	go func() {
		metricsAddr := cfg.MetricsListen
		logger.Info("starting metrics server", "addr", metricsAddr)
		http.Handle("/metrics", promhttp.Handler())
		if err := http.ListenAndServe(metricsAddr, nil); err != nil {
			logger.Error("metrics server failed", "error", err)
		}
	}()

	// start admin page endpoint
	go func() {
		logger.Info("starting admin server", "addr", cfg.AdminListen)
		if err := http.ListenAndServe(cfg.AdminListen, adminpage.Router()); err != nil {
			logger.Error("admin server failed", "error", err)
		}
	}()

	tap.Start(ctx)

	resyncer.Start(ctx)

	// periodically crawl the entire network to mirror the repositories
	crawler.Start(ctx)

	// listen to knotstream (currently we don't have relay for knots, so subscribe every known knots)
	knotstream.Start(ctx)

	svcErr := make(chan error, 1)
	if err := knotstream.ResubscribeAllHosts(ctx); err != nil {
		svcErr <- fmt.Errorf("resubscribing known hosts: %w", err)
	}

	logger.Info("startup complete")
	select {
	case <-ctx.Done():
		logger.Info("received shutdown signal", "reason", ctx.Err())
	case err := <-svcErr:
		if err != nil {
			logger.Error("service error", "error", err)
		}
		cancel()
	}

	logger.Info("shutting down knotmirror")
	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutdownCancel()

	var errs []error
	if err := knotstream.Shutdown(shutdownCtx); err != nil {
		errs = append(errs, err)
	}
	if err := db.Close(); err != nil {
		errs = append(errs, err)
	}
	for _, err := range errs {
		logger.Error("error during shutdown", "err", err)
	}

	logger.Info("shutdown complete")
	return nil
}
