package models

import "time"

type DomainClaim struct {
	ID      int64
	Did     string
	Domain  string
	Deleted *time.Time
}

type RepoSite struct {
	ID       int64
	RepoAt   string
	RepoName string // populated when joined with repos table
	Branch   string
	Dir      string
	IsIndex  bool
	Created  time.Time
	Updated  time.Time
}
