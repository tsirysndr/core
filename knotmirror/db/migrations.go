package db

import (
	"context"
	"database/sql"
	"fmt"
	"log/slog"
)

type MigrationFn = func(context.Context, *sql.Tx) error

type Migration struct {
	Name string
	Fn   MigrationFn
}

func ensureMigrationsTable(ctx context.Context, conn *sql.Conn) error {
	_, err := conn.ExecContext(ctx, `
		create table if not exists migrations (
			name text primary key,
			applied_at timestamptz not null default now()
		);
	`)
	return err
}

func RunMigration(ctx context.Context, conn *sql.Conn, logger *slog.Logger, m Migration) error {
	logger = logger.With("migration", m.Name)

	tx, err := conn.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("begin migration tx: %w", err)
	}
	defer tx.Rollback()

	var exists bool
	if err := tx.QueryRowContext(ctx, `select exists (select 1 from migrations where name = $1)`, m.Name).Scan(&exists); err != nil {
		return fmt.Errorf("checking migration state: %w", err)
	}
	if exists {
		logger.Debug("migration already applied")
		return nil
	}

	if err := m.Fn(ctx, tx); err != nil {
		logger.Error("migration failed", "err", err)
		return fmt.Errorf("running migration %s: %w", m.Name, err)
	}

	if _, err := tx.ExecContext(ctx, `insert into migrations (name) values ($1)`, m.Name); err != nil {
		return fmt.Errorf("recording migration: %w", err)
	}

	if err := tx.Commit(); err != nil {
		return fmt.Errorf("commit migration: %w", err)
	}

	logger.Info("migration applied")
	return nil
}

func RunMigrations(ctx context.Context, conn *sql.Conn, logger *slog.Logger, ms []Migration) error {
	if err := ensureMigrationsTable(ctx, conn); err != nil {
		return fmt.Errorf("ensuring migrations table: %w", err)
	}
	for _, m := range ms {
		if err := RunMigration(ctx, conn, logger, m); err != nil {
			return err
		}
	}
	return nil
}
