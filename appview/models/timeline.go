package models

import "time"

type TimelineEvent struct {
	*Repo
	*Follow
	*RepoStar

	EventAt time.Time

	// optional: populate only if Repo is a fork
	Source *Repo

	// optional: populate only if event is Follow
	*Profile
	*FollowStats
	*FollowStatus

	// optional: populate only if event is Repo
	IsStarred bool
	StarCount int64
}

// TimelineGroup is a primary TimelineEvent plus zero or more peer events
// that share the same operation+target (same repo starred, same user
// followed) and arrived consecutively. Primary is the newest of the group;
// Others holds the older peers in descending order. For non-collapsible
// events (repo create) Others is always empty.
type TimelineGroup struct {
	Primary TimelineEvent
	Others  []TimelineEvent
}

func (g TimelineGroup) IsCollapsed() bool {
	return len(g.Others) > 0
}

func (g TimelineGroup) OthersCount() int {
	return len(g.Others)
}
