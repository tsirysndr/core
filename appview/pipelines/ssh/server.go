package ssh

import (
	"context"
	"fmt"
	"log/slog"

	"github.com/charmbracelet/ssh"
	"github.com/charmbracelet/wish"
	tea "github.com/charmbracelet/wish/bubbletea"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/pipelines"
)

type Server struct {
	db               *db.DB
	config           *config.Config
	pipelineNotifier *pipelines.StatusNotifier
	logger           *slog.Logger
}

func New(db *db.DB, cfg *config.Config, pn *pipelines.StatusNotifier, logger *slog.Logger) *Server {
	return &Server{db: db, config: cfg, pipelineNotifier: pn, logger: logger}
}

func (s *Server) ListenAndServe(ctx context.Context) error {
	opts := []ssh.Option{
		wish.WithAddress(s.config.SSH.ListenAddr),
		wish.WithMiddleware(
			tea.Middleware(s.teaHandler),
			requirePty,
		),
	}

	if s.config.SSH.HostKeyPath != "" {
		opts = append(opts, wish.WithHostKeyPath(s.config.SSH.HostKeyPath))
	}

	srv, err := wish.NewServer(opts...)
	if err != nil {
		return err
	}

	go func() {
		<-ctx.Done()
		s.logger.Info("shutting down SSH log server")
		srv.Close()
	}()

	s.logger.Info("SSH log server listening", "address", s.config.SSH.ListenAddr)
	if err := srv.ListenAndServe(); err != ssh.ErrServerClosed {
		return err
	}
	s.logger.Info("SSH log server stopped")
	return nil
}

// requirePty is a middleware that rejects connections without a PTY and tells the user to pass -t.
func requirePty(next ssh.Handler) ssh.Handler {
	return func(sess ssh.Session) {
		_, _, ok := sess.Pty()
		if !ok {
			fmt.Fprintf(sess.Stderr(), "error: no terminal allocated\nhint: use `ssh -t` to force PTY allocation\n")
			sess.Exit(1)
			return
		}
		next(sess)
	}
}
