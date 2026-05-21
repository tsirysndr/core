package cursor

import (
	"database/sql"
	"errors"
	"fmt"
	"log/slog"

	_ "github.com/mattn/go-sqlite3"
)

type SqliteStore struct {
	db        *sql.DB
	tableName string
}

type SqliteStoreOpt func(*SqliteStore)

func WithTableName(name string) SqliteStoreOpt {
	return func(s *SqliteStore) {
		s.tableName = name
	}
}

func NewSQLiteStore(dbPath string, opts ...SqliteStoreOpt) (*SqliteStore, error) {
	db, err := sql.Open("sqlite3", dbPath+"?_foreign_keys=1")
	if err != nil {
		return nil, fmt.Errorf("failed to open sqlite database: %w", err)
	}

	store := &SqliteStore{
		db:        db,
		tableName: "cursors",
	}

	for _, o := range opts {
		o(store)
	}

	if err := store.init(); err != nil {
		return nil, err
	}

	return store, nil
}

func (s *SqliteStore) init() error {
	createTable := fmt.Sprintf(`
	create table if not exists %s (
		knot text primary key,
		cursor integer
	);`, s.tableName)
	_, err := s.db.Exec(createTable)
	return err
}

func (s *SqliteStore) Set(key string, cursor int64) {
	query := fmt.Sprintf(`
		insert into %s (knot, cursor)
		values (?, ?)
		on conflict(knot) do update set cursor=excluded.cursor;
	`, s.tableName)

	if _, err := s.db.Exec(query, key, cursor); err != nil {
		slog.Default().Error("cursor sqlite set failed", "key", key, "cursor", cursor, "err", err)
	}
}

func (s *SqliteStore) Get(key string) (cursor int64) {
	query := fmt.Sprintf(`
		select cursor from %s where knot = ?;
	`, s.tableName)
	err := s.db.QueryRow(query, key).Scan(&cursor)

	if err != nil {
		if !errors.Is(err, sql.ErrNoRows) {
			slog.Default().Error("cursor sqlite get failed", "key", key, "err", err)
		}
		return 0
	}

	return cursor
}
