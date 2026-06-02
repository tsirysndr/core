package db

import (
	"encoding/json"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
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

func (d *DB) EmitKnotMemberUpdate(n *notifier.Notifier, op AclOp, subject syntax.DID) error {
	payload := KnotMemberUpdate{
		Op:      op,
		Subject: subject.String(),
	}

	eventJson, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("marshal memberUpdate event: %w", err)
	}

	return d.InsertEvent(eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      KnotMemberUpdateNSID,
		EventJson: eventJson,
	}, n)
}

func (d *DB) EmitCollaboratorUpdate(n *notifier.Notifier, op AclOp, subject, repoDid syntax.DID) error {
	payload := RepoCollaboratorUpdate{
		Op:      op,
		Subject: subject.String(),
		Repo:    repoDid.String(),
	}

	eventJson, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("marshal collaboratorUpdate event: %w", err)
	}

	return d.InsertEvent(eventstream.Event{
		Rkey:      tid.TID(),
		Nsid:      RepoCollaboratorUpdateNSID,
		EventJson: eventJson,
	}, n)
}

func (d *DB) GetEvents(cursor int64, limit int) ([]eventstream.Event, error) {
	return eventstream.List(d.db, cursor, limit)
}
