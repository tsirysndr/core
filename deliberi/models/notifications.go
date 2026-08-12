package models

import (
	"context"
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/idresolver"
)

type NotificationType string

const (
	NotificationTypeRepoStarred     NotificationType = "repo_starred"
	NotificationTypeIssueCreated    NotificationType = "issue_created"
	NotificationTypeIssueCommented  NotificationType = "issue_commented"
	NotificationTypePullCreated     NotificationType = "pull_created"
	NotificationTypePullCommented   NotificationType = "pull_commented"
	NotificationTypeFollowed        NotificationType = "followed"
	NotificationTypePullMerged      NotificationType = "pull_merged"
	NotificationTypeIssueClosed     NotificationType = "issue_closed"
	NotificationTypeIssueReopen     NotificationType = "issue_reopen"
	NotificationTypePullClosed      NotificationType = "pull_closed"
	NotificationTypePullReopen      NotificationType = "pull_reopen"
	NotificationTypeUserMentioned   NotificationType = "user_mentioned"
	NotificationTypeIssueAssigned   NotificationType = "issue_assigned"
	NotificationTypeIssueUnassigned NotificationType = "issue_unassigned"
	NotificationTypePullAssigned    NotificationType = "pull_assigned"
	NotificationTypePullUnassigned  NotificationType = "pull_unassigned"
)

var SocialNotificationTypes = []NotificationType{
	NotificationTypeRepoStarred,
	NotificationTypeFollowed,
}

var WorkNotificationTypes = []NotificationType{
	NotificationTypeIssueCreated,
	NotificationTypeIssueCommented,
	NotificationTypeIssueClosed,
	NotificationTypeIssueReopen,
	NotificationTypePullCreated,
	NotificationTypePullCommented,
	NotificationTypePullMerged,
	NotificationTypePullClosed,
	NotificationTypePullReopen,
	NotificationTypeUserMentioned,
	NotificationTypeIssueAssigned,
	NotificationTypeIssueUnassigned,
	NotificationTypePullAssigned,
	NotificationTypePullUnassigned,
}

// email digest types; social types (repo_starred, followed) are excluded.
var EmailNotificationTypes = []NotificationType{
	NotificationTypeIssueCreated,
	NotificationTypeIssueCommented,
	NotificationTypeIssueClosed,
	NotificationTypeIssueReopen,
	NotificationTypePullCreated,
	NotificationTypePullCommented,
	NotificationTypePullMerged,
	NotificationTypePullClosed,
	NotificationTypePullReopen,
	NotificationTypeUserMentioned,
	NotificationTypeIssueAssigned,
	NotificationTypeIssueUnassigned,
	NotificationTypePullAssigned,
	NotificationTypePullUnassigned,
}

type Notification struct {
	ID           int64
	RecipientDid string
	AtUri        string // source record (comment/issue/pull/star/follow); dedupe key
	Type         NotificationType
	ActorDid     string
	RepoDid      string
	RepoName     string // not stored; filled from the repo cache when the digest reads
	EntityAt     string // related issue/pull at-uri (for links); source itself for issue/pull
	EntityTitle  string
	Read         bool
	Emailed      bool
	Created      time.Time
}

func (n *Notification) Icon() string {
	switch n.Type {
	case NotificationTypeRepoStarred:
		return "star"
	case NotificationTypeIssueCreated, NotificationTypeIssueReopen:
		return "circle-dot"
	case NotificationTypeIssueCommented, NotificationTypePullCommented:
		return "message-square"
	case NotificationTypeIssueClosed:
		return "ban"
	case NotificationTypePullCreated, NotificationTypePullReopen:
		return "git-pull-request-create"
	case NotificationTypePullMerged:
		return "git-merge"
	case NotificationTypePullClosed:
		return "git-pull-request-closed"
	case NotificationTypeFollowed:
		return "user-plus"
	case NotificationTypeUserMentioned:
		return "at-sign"
	case NotificationTypeIssueAssigned, NotificationTypePullAssigned:
		return "user-round-arrow-forward"
	case NotificationTypeIssueUnassigned, NotificationTypePullUnassigned:
		return "user-round-minus"
	default:
		return ""
	}
}

func (n *Notification) URL(res *idresolver.Resolver) string {
	resolve := func(did string) string {
		if id, err := res.ResolveIdent(context.Background(), did); err == nil && !id.Handle.IsInvalidHandle() {
			return id.Handle.String()
		}
		return did
	}

	if n.Type == NotificationTypeFollowed {
		return "/" + resolve(n.ActorDid)
	}
	if n.RepoDid == "" || n.RepoName == "" {
		return ""
	}
	repoHandle := resolve(n.RepoDid)
	if n.EntityAt != "" {
		switch syntax.ATURI(n.EntityAt).Collection().String() {
		case "sh.tangled.repo.issue":
			return fmt.Sprintf("/%s/%s/issues/%s", repoHandle, n.RepoName, n.EntityAt)
		case "sh.tangled.repo.pull":
			return fmt.Sprintf("/%s/%s/pulls/%s", repoHandle, n.RepoName, n.EntityAt)
		}
	}
	return fmt.Sprintf("/%s/%s", repoHandle, n.RepoName)
}

func Category(t NotificationType) string {
	for _, st := range SocialNotificationTypes {
		if st == t {
			return "social"
		}
	}
	return "work"
}

type NotificationPreferences struct {
	ID                 int64
	UserDid            syntax.DID
	RepoStarred        bool
	IssueCreated       bool
	IssueCommented     bool
	PullCreated        bool
	PullCommented      bool
	Followed           bool
	UserMentioned      bool
	PullMerged         bool
	IssueClosed        bool
	EmailNotifications bool
}

func (prefs *NotificationPreferences) ShouldNotify(t NotificationType) bool {
	switch t {
	case NotificationTypeRepoStarred:
		return prefs.RepoStarred
	case NotificationTypeIssueCreated:
		return prefs.IssueCreated
	case NotificationTypeIssueCommented:
		return prefs.IssueCommented
	case NotificationTypeIssueClosed:
		return prefs.IssueClosed
	case NotificationTypeIssueReopen:
		return prefs.IssueCreated
	case NotificationTypePullCreated:
		return prefs.PullCreated
	case NotificationTypePullCommented:
		return prefs.PullCommented
	case NotificationTypePullMerged:
		return prefs.PullMerged
	case NotificationTypePullClosed:
		return prefs.PullMerged
	case NotificationTypePullReopen:
		return prefs.PullCreated
	case NotificationTypeFollowed:
		return prefs.Followed
	case NotificationTypeUserMentioned,
		NotificationTypeIssueAssigned,
		NotificationTypeIssueUnassigned,
		NotificationTypePullAssigned,
		NotificationTypePullUnassigned:
		return prefs.UserMentioned
	default:
		return false
	}
}

func DefaultNotificationPreferences(user syntax.DID) *NotificationPreferences {
	return &NotificationPreferences{
		UserDid:            user,
		RepoStarred:        true,
		IssueCreated:       true,
		IssueCommented:     true,
		PullCreated:        true,
		PullCommented:      true,
		Followed:           true,
		UserMentioned:      true,
		PullMerged:         true,
		IssueClosed:        true,
		EmailNotifications: false,
	}
}
