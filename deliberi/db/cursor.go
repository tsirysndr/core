package db

import "fmt"

func (d *DB) GetLastTimeUs() (int64, error) {
	var t int64
	if err := d.QueryRow(`select last_time_us from jetstream_cursor where id = 0`).Scan(&t); err != nil {
		return 0, fmt.Errorf("no saved cursor: %w", err)
	}
	return t, nil
}

func (d *DB) SaveLastTimeUs(t int64) error {
	_, err := d.Exec(
		`insert into jetstream_cursor (id, last_time_us) values (0, ?)
		 on conflict(id) do update set last_time_us = excluded.last_time_us`,
		t,
	)
	return err
}
