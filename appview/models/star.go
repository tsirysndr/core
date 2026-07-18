package models

import (
	"time"
)

type StarSubjectType string

const (
	StarSubjectRepo   StarSubjectType = "repo"
	StarSubjectString StarSubjectType = "string"
)

type Star struct {
	Did         string
	SubjectType StarSubjectType
	Subject     string
	Created     time.Time
}

// RepoStar is used for reverse mapping to repos
type RepoStar struct {
	Star
	Repo *Repo
}

// StringStar is used for reverse mapping to strings
type StringStar struct {
	Star
	String *String
}
