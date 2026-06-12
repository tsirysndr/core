package db

import (
	"time"
)

type NixOSToplevelCacheRecord struct {
	ConfigKey string
	Toplevel  string
	UpdatedAt time.Time
}

func (d *DB) GetNixOSToplevelCacheRecord(configKey string) (*NixOSToplevelCacheRecord, error) {
	var record NixOSToplevelCacheRecord
	var updatedAtStr string
	err := d.QueryRow(
		`select config_key, toplevel, updated_at from nixos_toplevel_cache where config_key = ?`,
		configKey,
	).Scan(&record.ConfigKey, &record.Toplevel, &updatedAtStr)
	if err != nil {
		return nil, err
	}
	updatedAt, err := time.Parse(time.RFC3339, updatedAtStr)
	if err != nil {
		return nil, err
	}
	record.UpdatedAt = updatedAt
	return &record, nil
}

func (d *DB) SaveNixOSToplevelCacheRecord(configKey, toplevel string) error {
	_, err := d.Exec(
		`insert into nixos_toplevel_cache (config_key, toplevel, updated_at)
		 values (?, ?, ?)
		 on conflict(config_key) do update set
		     toplevel   = excluded.toplevel,
		     updated_at = excluded.updated_at`,
		configKey, toplevel, time.Now().UTC().Format(time.RFC3339),
	)
	return err
}
