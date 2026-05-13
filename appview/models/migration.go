package models

import (
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type PDSRecordMigration struct {
	Did      syntax.DID
	Name     string         // name of the migration
	Records  []syntax.ATURI // records that need a migration
	ErrorMsg *string        // error message from previous attempt
}

type PDSMigration struct {
	Name       string           // name of the migration
	Did        syntax.DID       // record owner
	Collection syntax.NSID      // record collection
	Rkey       syntax.RecordKey // record rkey
	Status     PDSMigrationStatus
	ErrorMsg   *string // error message from previous attempt
	RetryCount int
	RetryAfter int64 // Unix timestamp (seconds)
}

type PDSMigrationStatus string

const (
	PDSMigrationStatusPending PDSMigrationStatus = "pending"
	PDSMigrationStatusRunning PDSMigrationStatus = "running"
	PDSMigrationStatusDone    PDSMigrationStatus = "done"
	PDSMigrationStatusFailed  PDSMigrationStatus = "failed"
)

func (m *PDSMigration) RecordAtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", m.Did, m.Collection, m.Rkey))
}
