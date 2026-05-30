package models

import "time"

type RecentLinkType string

const (
	RecentLinkTypeRepo  RecentLinkType = "repo"
	RecentLinkTypeIssue RecentLinkType = "issue"
	RecentLinkTypePull  RecentLinkType = "pull"
)

type RecentLink struct {
	Id       int64
	UserDid  string
	LinkType RecentLinkType
	Target   string // repo DID for repos; AT-URI string for issues/pulls
	Visited  time.Time
}
