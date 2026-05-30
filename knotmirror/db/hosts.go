package db

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log"

	"tangled.org/core/knotmirror/models"
)

func UpsertHost(ctx context.Context, e DBTX, host *models.Host) error {
	if _, err := e.ExecContext(ctx,
		`insert into hosts (hostname, no_ssl, status, last_seq)
		values ($1, $2, $3, $4)
		on conflict(hostname) do update set
			no_ssl   = excluded.no_ssl,
			status   = excluded.status,
			last_seq = excluded.last_seq
		`,
		host.Hostname,
		host.NoSSL,
		host.Status,
		host.LastSeq,
	); err != nil {
		return fmt.Errorf("upserting host: %w", err)
	}
	return nil
}

func GetHost(ctx context.Context, e DBTX, hostname string) (*models.Host, error) {
	var host models.Host
	if err := e.QueryRowContext(ctx,
		`select hostname, no_ssl, status, last_seq
		from hosts where hostname = $1`,
		hostname,
	).Scan(
		&host.Hostname,
		&host.NoSSL,
		&host.Status,
		&host.LastSeq,
	); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, err
	}
	return &host, nil
}

func StoreCursors(ctx context.Context, e *sql.DB, cursors []models.HostCursor) error {
	tx, err := e.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("starting transaction: %w", err)
	}
	defer tx.Rollback()
	for _, cur := range cursors {
		if cur.LastSeq <= 0 {
			continue
		}
		if _, err := tx.ExecContext(ctx,
			`update hosts set last_seq = $1 where hostname = $2`,
			cur.LastSeq,
			cur.Hostname,
		); err != nil {
			log.Println("failed to persist host cursor", "host", cur.Hostname, "lastSeq", cur.LastSeq, "err", err)
		}
	}
	return tx.Commit()
}

func ListHosts(ctx context.Context, e DBTX, status models.HostStatus) ([]models.Host, error) {
	rows, err := e.QueryContext(ctx,
		`select hostname, no_ssl, status, last_seq
		from hosts
		where status = $1`,
		status,
	)
	if err != nil {
		return nil, fmt.Errorf("querying hosts: %w", err)
	}
	defer rows.Close()

	var hosts []models.Host
	for rows.Next() {
		var host models.Host
		if err := rows.Scan(
			&host.Hostname,
			&host.NoSSL,
			&host.Status,
			&host.LastSeq,
		); err != nil {
			return nil, fmt.Errorf("scanning row: %w", err)
		}
		hosts = append(hosts, host)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("scanning rows: %w ", err)
	}
	return hosts, nil
}
