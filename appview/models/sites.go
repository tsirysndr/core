package models

import (
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type DomainClaim struct {
	ID      int64
	Did     string
	Domain  string
	Deleted *time.Time
}

type RepoSite struct {
	ID       int64
	RepoDid  syntax.DID
	RepoRkey string // populated when joined with repos table
	Branch   string
	Dir      string
	IsIndex  bool
	Created  time.Time
	Updated  time.Time
}
