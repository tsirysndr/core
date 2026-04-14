package models

import (
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
)

type Repo struct {
	Did  syntax.DID
	Rkey syntax.RecordKey
	Cid  *syntax.CID
	// content of tangled.Repo
	Name       string
	KnotDomain string
	RepoDid    syntax.DID

	GitRev     syntax.TID // last processed git.refUpdate revision
	RepoSha    string     // sha256 sum of git refs (to avoid no-op git fetch)
	State      RepoState
	ErrorMsg   string
	RetryCount int
	RetryAfter int64 // Unix timestamp (seconds)
}

func (r *Repo) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", r.Did, tangled.RepoNSID, r.Rkey))
}

func (r *Repo) RepoIdentifier() string {
	return r.RepoDid.String()
}

type RepoState string

const (
	RepoStatePending        RepoState = "pending"
	RepoStateDesynchronized RepoState = "desynchronized"
	RepoStateResyncing      RepoState = "resyncing"
	RepoStateActive         RepoState = "active"
	RepoStateSuspended      RepoState = "suspended"
	RepoStateError          RepoState = "error"
)

var AllRepoStates = []RepoState{
	RepoStatePending,
	RepoStateDesynchronized,
	RepoStateResyncing,
	RepoStateActive,
	RepoStateSuspended,
	RepoStateError,
}

func (s RepoState) IsResyncing() bool {
	return s == RepoStateResyncing
}

type HostCursor struct {
	Hostname string
	LastSeq  int64
}

type Host struct {
	Hostname string
	NoSSL    bool
	Status   HostStatus
	LastSeq  int64
}

type HostStatus string

const (
	HostStatusActive    HostStatus = "active"
	HostStatusIdle      HostStatus = "idle"
	HostStatusOffline   HostStatus = "offline"
	HostStatusThrottled HostStatus = "throttled"
	HostStatusBanned    HostStatus = "banned"
)

var AllHostStatuses = []HostStatus{
	HostStatusActive,
	HostStatusIdle,
	HostStatusOffline,
	HostStatusThrottled,
	HostStatusBanned,
}

func (h *Host) URL() string {
	if h.NoSSL {
		return fmt.Sprintf("http://%s", h.Hostname)
	} else {
		return fmt.Sprintf("https://%s", h.Hostname)
	}
}

func (h *Host) WsURL() string {
	if h.NoSSL {
		return fmt.Sprintf("ws://%s", h.Hostname)
	} else {
		return fmt.Sprintf("wss://%s", h.Hostname)
	}
}

// func (h *Host) SubscribeGitRefsURL(cursor int64) string {
// 	scheme := "wss"
// 	if h.NoSSL {
// 		scheme = "ws"
// 	}
// 	u := fmt.Sprintf("%s://%s/xrpc/%s", scheme, h.Hostname, tangled.SubscribeGitRefsNSID)
// 	if cursor > 0 {
// 		u = fmt.Sprintf("%s?cursor=%d", u, h.LastSeq)
// 	}
// 	return u
// }

func (h *Host) LegacyEventsURL(cursor int64) string {
	u := fmt.Sprintf("%s/events", h.WsURL())
	if cursor > 0 {
		u = fmt.Sprintf("%s?cursor=%d", u, cursor)
	}
	return u
}
