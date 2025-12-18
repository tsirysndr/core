package tapc

import (
	"encoding/json"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type EventType string

const (
	EvtRecord   EventType = "record"
	EvtIdentity EventType = "identity"
)

type Event struct {
	ID       int64              `json:"id"`
	Type     EventType          `json:"type"`
	Record   *RecordEventData   `json:"record,omitempty"`
	Identity *IdentityEventData `json:"identity,omitempty"`
}

type RecordEventData struct {
	Live       bool             `json:"live"`
	Did        syntax.DID       `json:"did"`
	Rev        string           `json:"rev"`
	Collection syntax.NSID      `json:"collection"`
	Rkey       syntax.RecordKey `json:"rkey"`
	Action     RecordAction     `json:"action"`
	Record     json.RawMessage  `json:"record,omitempty"`
	CID        *syntax.CID      `json:"cid,omitempty"`
}

func (r *RecordEventData) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", r.Did, r.Collection, r.Rkey))
}

type RecordAction string

const (
	RecordCreateAction RecordAction = "create"
	RecordUpdateAction RecordAction = "update"
	RecordDeleteAction RecordAction = "delete"
)

type IdentityEventData struct {
	DID      syntax.DID `json:"did"`
	Handle   string     `json:"handle"`
	IsActive bool       `json:"is_active"`
	Status   RepoStatus `json:"status"`
}

type RepoStatus string

const (
	RepoStatusActive      RepoStatus = "active"
	RepoStatusTakendown   RepoStatus = "takendown"
	RepoStatusSuspended   RepoStatus = "suspended"
	RepoStatusDeactivated RepoStatus = "deactivated"
	RepoStatusDeleted     RepoStatus = "deleted"
)
