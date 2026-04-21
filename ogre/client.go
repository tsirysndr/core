package ogre

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"
)

type Client struct {
	host   string
	client *http.Client
}

func NewClient(host string) *Client {
	return &Client{
		host: host,
		client: &http.Client{
			Timeout: 10 * time.Second,
		},
	}
}

type LabelData struct {
	Name  string `json:"name"`
	Color string `json:"color"`
}

type LanguageData struct {
	Color      string  `json:"color"`
	Percentage float32 `json:"percentage"`
}

type RepositoryCardPayload struct {
	Type        string         `json:"type"`
	RepoName    string         `json:"repoName"`
	OwnerHandle string         `json:"ownerHandle"`
	Stars       int            `json:"stars"`
	Pulls       int            `json:"pulls"`
	Issues      int            `json:"issues"`
	CreatedAt   string         `json:"createdAt"`
	AvatarUrl   string         `json:"avatarUrl"`
	Languages   []LanguageData `json:"languages"`
}

type IssueCardPayload struct {
	Type            string      `json:"type"`
	RepoName        string      `json:"repoName"`
	OwnerHandle     string      `json:"ownerHandle"`
	AuthorHandle    string      `json:"authorHandle"`
	AvatarUrl       string      `json:"avatarUrl"`
	AuthorAvatarUrl string      `json:"authorAvatarUrl"`
	Title           string      `json:"title"`
	IssueNumber     int         `json:"issueNumber"`
	Status          string      `json:"status"`
	Labels          []LabelData `json:"labels"`
	CommentCount    int         `json:"commentCount"`
	ReactionCount   int         `json:"reactionCount"`
	CreatedAt       string      `json:"createdAt"`
}

type PullRequestCardPayload struct {
	Type              string `json:"type"`
	RepoName          string `json:"repoName"`
	OwnerHandle       string `json:"ownerHandle"`
	AuthorHandle      string `json:"authorHandle"`
	AvatarUrl         string `json:"avatarUrl"`
	AuthorAvatarUrl   string `json:"authorAvatarUrl"`
	Title             string `json:"title"`
	PullRequestNumber int    `json:"pullRequestNumber"`
	Status            string `json:"status"`
	FilesChanged      int    `json:"filesChanged"`
	Additions         int    `json:"additions"`
	Deletions         int    `json:"deletions"`
	Rounds            int    `json:"rounds"`
	CommentCount      int    `json:"commentCount"`
	ReactionCount     int    `json:"reactionCount"`
	CreatedAt         string `json:"createdAt"`
}

func (c *Client) doRequest(ctx context.Context, path string, payload any) ([]byte, error) {
	body, err := json.Marshal(payload)
	if err != nil {
		return nil, fmt.Errorf("marshal payload: %w", err)
	}

	url := fmt.Sprintf("%s/%s", c.host, path)
	req, err := http.NewRequestWithContext(ctx, "POST", url, bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("do request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		respBody, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("unexpected status: %d, body: %s", resp.StatusCode, string(respBody))
	}

	return io.ReadAll(resp.Body)
}

func (c *Client) RenderRepositoryCard(ctx context.Context, payload RepositoryCardPayload) ([]byte, error) {
	return c.doRequest(ctx, "repository", payload)
}

func (c *Client) RenderIssueCard(ctx context.Context, payload IssueCardPayload) ([]byte, error) {
	return c.doRequest(ctx, "issue", payload)
}

func (c *Client) RenderPullRequestCard(ctx context.Context, payload PullRequestCardPayload) ([]byte, error) {
	return c.doRequest(ctx, "pullRequest", payload)
}
