package models

import "github.com/bluesky-social/indigo/atproto/syntax"

type RefKind int

const (
	RefKindIssue RefKind = iota
	RefKindPull
)

func (k RefKind) String() string {
	if k == RefKindIssue {
		return "issues"
	} else {
		return "pulls"
	}
}

// /@alice.com/cool-proj/issues/123
// /@alice.com/cool-proj/issues/123#comment-3mleetx5lhz22
type ReferenceLink struct {
	Handle      string
	Repo        string
	Kind        RefKind
	SubjectId   int
	CommentRkey *syntax.RecordKey
}

type RichReferenceLink struct {
	ReferenceLink
	Title string
	// reusing PullState for both issue & PR
	State PullState
}
