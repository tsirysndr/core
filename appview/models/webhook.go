package models

import (
	"slices"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type WebhookEvent string

const (
	WebhookEventPush                   WebhookEvent = "push"
	WebhookEventRepoRenamed            WebhookEvent = "repository:renamed"
	WebhookEventPullRequestCreated     WebhookEvent = "pull_request:created"
	WebhookEventPullRequestResubmitted WebhookEvent = "pull_request:resubmitted"
	WebhookEventPullRequestMerged      WebhookEvent = "pull_request:merged"
	WebhookEventPullRequestClosed      WebhookEvent = "pull_request:closed"
	WebhookEventPullRequestReopened    WebhookEvent = "pull_request:reopened"
)

type Webhook struct {
	Id        int64
	RepoDid   syntax.DID
	Url       string
	Secret    string
	Active    bool
	Events    []string // comma-separated event types
	CreatedAt time.Time
	UpdatedAt time.Time
}

// HasEvent checks if the webhook is subscribed to a specific event
func (w *Webhook) HasEvent(event WebhookEvent) bool {
	return slices.Contains(w.Events, string(event))
}

type WebhookDelivery struct {
	Id           int64
	WebhookId    int64
	Event        string
	DeliveryId   string // UUID for tracking
	Url          string
	RequestBody  string
	ResponseCode int
	ResponseBody string
	Success      bool
	CreatedAt    time.Time
}

// WebhookPayload represents the webhook payload structure
type WebhookPayload struct {
	Ref        string            `json:"ref"`
	Before     string            `json:"before"`
	After      string            `json:"after"`
	Repository WebhookRepository `json:"repository"`
	Pusher     WebhookUser       `json:"pusher"`
}

// WebhookRepository represents repository information in webhook payload
type WebhookRepository struct {
	Name        string      `json:"name"`
	FullName    string      `json:"full_name"`
	Description string      `json:"description"`
	Fork        bool        `json:"fork"`
	HtmlUrl     string      `json:"html_url"`
	CloneUrl    string      `json:"clone_url"`
	SshUrl      string      `json:"ssh_url"`
	Website     string      `json:"website,omitempty"`
	StarsCount  int         `json:"stars_count,omitempty"`
	OpenIssues  int         `json:"open_issues_count,omitempty"`
	CreatedAt   string      `json:"created_at"`
	UpdatedAt   string      `json:"updated_at"`
	Owner       WebhookUser `json:"owner"`
}

// WebhookUser represents user information in webhook payload
type WebhookUser struct {
	Did string `json:"did"`
}

// WebhookRenamePayload represents the payload for a repository:renamed event
type WebhookRenamePayload struct {
	OldName    string            `json:"old_name"`
	NewName    string            `json:"new_name"`
	Repository WebhookRepository `json:"repository"`
	Sender     WebhookUser       `json:"sender"`
}

// WebhookPullRequestPayload represents the payload for pull_request:* events
type WebhookPullRequestPayload struct {
	Action      string             `json:"action"`
	PullRequest WebhookPullRequest `json:"pull_request"`
	Repository  WebhookRepository  `json:"repository"`
	Sender      WebhookUser        `json:"sender"`
}

// WebhookPullRequest represents pull request information in webhook payload
type WebhookPullRequest struct {
	Number       int                       `json:"number"`
	Title        string                    `json:"title"`
	Body         string                    `json:"body"`
	State        string                    `json:"state"`
	TargetBranch string                    `json:"target_branch"`
	Source       *WebhookPullRequestSource `json:"source,omitempty"`
	RoundNumber  int                       `json:"round_number"`
	Owner        WebhookUser               `json:"owner"`
	HtmlUrl      string                    `json:"html_url"`
	PatchUrl     string                    `json:"patch_url"`
	CreatedAt    string                    `json:"created_at"`
}

// WebhookPullRequestSource represents the source of a branch- or fork-based
// pull request; absent for patch-based pull requests
type WebhookPullRequestSource struct {
	Branch string `json:"branch"`
	Repo   string `json:"repo,omitempty"`
	Sha    string `json:"sha,omitempty"`
}
