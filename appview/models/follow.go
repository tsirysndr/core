package models

import (
	"time"

	"tangled.org/core/api/tangled"
)

type Follow struct {
	UserDid    string
	SubjectDid string
	FollowedAt time.Time
}

func (f *Follow) AsRecord() tangled.GraphFollow {
	return tangled.GraphFollow{
		Subject:   f.SubjectDid,
		CreatedAt: f.FollowedAt.Format(time.RFC3339),
	}
}

type FollowStats struct {
	Followers int64
	Following int64
}

type FollowStatus int

const (
	IsNotFollowing FollowStatus = iota
	IsFollowing
	IsSelf
)

func (s FollowStatus) String() string {
	switch s {
	case IsNotFollowing:
		return "IsNotFollowing"
	case IsFollowing:
		return "IsFollowing"
	case IsSelf:
		return "IsSelf"
	default:
		return "IsNotFollowing"
	}
}
