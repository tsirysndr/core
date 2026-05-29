package models

import (
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/api/tangled"
)

type ReactionKind string

const (
	Like        ReactionKind = "👍"
	Unlike      ReactionKind = "👎"
	Laugh       ReactionKind = "😆"
	Celebration ReactionKind = "🎉"
	Confused    ReactionKind = "🫤"
	Heart       ReactionKind = "❤️"
	Rocket      ReactionKind = "🚀"
	Eyes        ReactionKind = "👀"
)

func (rk ReactionKind) String() string {
	return string(rk)
}

var OrderedReactionKinds = []ReactionKind{
	Like,
	Unlike,
	Laugh,
	Celebration,
	Confused,
	Heart,
	Rocket,
	Eyes,
}

func ParseReactionKind(raw string) (ReactionKind, bool) {
	k, ok := (map[string]ReactionKind{
		"👍":  Like,
		"👎":  Unlike,
		"😆":  Laugh,
		"🎉":  Celebration,
		"🫤":  Confused,
		"❤️": Heart,
		"🚀":  Rocket,
		"👀":  Eyes,
	})[raw]
	return k, ok
}

type Reaction struct {
	ReactedByDid string
	ThreadAt     syntax.ATURI
	Created      time.Time
	Rkey         string
	Kind         ReactionKind
}

type ReactionDisplayData struct {
	Count int
	Users []string
}

func NormalizeReactionSubject(subject syntax.ATURI) syntax.ATURI {
	switch subject.Collection() {
	case tangled.RepoIssueCommentNSID, tangled.RepoPullCommentNSID:
		return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", subject.Authority(), tangled.FeedCommentNSID, subject.RecordKey()))
	}
	return subject
}
