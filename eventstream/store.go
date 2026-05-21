package eventstream

import (
	"database/sql"
	"encoding/json"
	"sync"
	"time"

	"tangled.org/core/notifier"
)

type Store interface {
	Exec(query string, args ...any) (sql.Result, error)
	Query(query string, args ...any) (*sql.Rows, error)
}

var (
	clockMu   sync.Mutex
	lastNanos int64
)

func Insert(s Store, ev Event, n *notifier.Notifier) error {
	clockMu.Lock()
	defer clockMu.Unlock()

	if ev.Created == 0 {
		now := time.Now().UnixNano()
		if now <= lastNanos {
			now = lastNanos + 1
		}
		ev.Created = now
	}
	if ev.Created > lastNanos {
		lastNanos = ev.Created
	}

	if _, err := s.Exec(
		`insert into events (rkey, nsid, event, created) values (?, ?, ?, ?)`,
		ev.Rkey,
		ev.Nsid,
		[]byte(ev.EventJson),
		ev.Created,
	); err != nil {
		return err
	}
	n.NotifyAll()
	return nil
}

func List(s Store, cursor int64, limit int) ([]Event, error) {
	rows, err := s.Query(`
		select rkey, nsid, event, created
		from events
		where created > ?
		order by created asc
		limit ?
	`, cursor, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []Event
	for rows.Next() {
		var ev Event
		var eventJsonStr string
		if err := rows.Scan(&ev.Rkey, &ev.Nsid, &eventJsonStr, &ev.Created); err != nil {
			return nil, err
		}
		ev.EventJson = json.RawMessage(eventJsonStr)
		out = append(out, ev)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return out, nil
}
