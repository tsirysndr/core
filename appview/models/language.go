package models

import "github.com/bluesky-social/indigo/atproto/syntax"

type RepoLanguage struct {
	Id           int64
	RepoDid      syntax.DID
	Ref          string
	IsDefaultRef bool
	Language     string
	Bytes        int64
}
