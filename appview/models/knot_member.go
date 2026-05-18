package models

import (
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type KnotMember struct {
	Id      int64
	Did     syntax.DID
	Rkey    string
	Domain  string
	Subject syntax.DID
	Created time.Time
}
