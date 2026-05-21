package db

import (
	"encoding/json"
	"fmt"

	"tangled.org/core/eventstream"
	"tangled.org/core/notifier"
	"tangled.org/core/tid"
)

func (d *DB) InsertEvent(event eventstream.Event, n *notifier.Notifier) error {
	return eventstream.Insert(d.db, event, n)
}

func (d *DB) EmitDIDAssign(n *notifier.Notifier, ownerDid, repoName, repoDid string) error {
	payload := RepoDIDAssign{
		OwnerDid: ownerDid,
		RepoName: repoName,
		RepoDid:  repoDid,
	}

	eventJson, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("marshal didAssign event: %w", err)
	}

	return d.InsertEvent(eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      RepoDIDAssignNSID,
		EventJson: eventJson,
	}, n)
}

func (d *DB) GetEvents(cursor int64, limit int) ([]eventstream.Event, error) {
	return eventstream.List(d.db, cursor, limit)
}
