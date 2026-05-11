package models

import (
	"database/sql"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type Collaborator struct {
	// identifiers for the record
	Id   int64
	Did  syntax.DID
	Rkey sql.NullString

	// content
	SubjectDid syntax.DID
	RepoDid    syntax.DID

	// meta
	Created time.Time
}
