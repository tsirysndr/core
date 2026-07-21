package webhook

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"time"

	"github.com/avast/retry-go/v4"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/google/uuid"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/hostutil"
	"tangled.org/core/log"
	"tangled.org/core/orm"
)

type Notifier struct {
	notify.BaseNotifier
	db      *db.DB
	baseUrl string
	logger  *slog.Logger
	client  *http.Client
}

func NewNotifier(database *db.DB, baseUrl string, dev bool) *Notifier {
	return &Notifier{
		db:      database,
		baseUrl: baseUrl,
		logger:  log.New("webhook-notifier"),
		// user-supplied webhook URLs are untrusted: block internal address
		// ranges and don't follow redirects to guard against SSRF.
		client: hostutil.SafeClient(dev, 30*time.Second),
	}
}

var _ notify.Notifier = &Notifier{}

func (w *Notifier) Push(ctx context.Context, repo *models.Repo, ref, oldSha, newSha, committerDid string) {
	webhooks, err := w.activeWebhooksForEvent(repo.RepoDid, models.WebhookEventPush)
	if err != nil {
		w.logger.Error("failed to get webhooks for repo", "repo_did", repo.RepoDid, "err", err)
		return
	}
	if len(webhooks) == 0 {
		return
	}

	payload := w.buildPushPayload(repo, ref, oldSha, newSha, committerDid)
	payloadBytes, err := json.Marshal(payload)
	if err != nil {
		w.logger.Error("failed to marshal push payload", "repo_did", repo.RepoDid, "err", err)
		return
	}

	userAgent := "Tangled-Hook/" + newSha[:7]
	for _, webhook := range webhooks {
		go w.sendWebhook(ctx, webhook, string(models.WebhookEventPush), payload.Repository.FullName, userAgent, payloadBytes)
	}
}

func (w *Notifier) RenameRepo(ctx context.Context, actor syntax.DID, oldRepo, newRepo *models.Repo) {
	webhooks, err := w.activeWebhooksForEvent(newRepo.RepoDid, models.WebhookEventRepoRenamed)
	if err != nil {
		w.logger.Error("failed to get webhooks for repo", "repo_did", newRepo.RepoDid, "err", err)
		return
	}
	if len(webhooks) == 0 {
		return
	}

	payload := &models.WebhookRenamePayload{
		OldName:    oldRepo.Name,
		NewName:    newRepo.Name,
		Repository: buildWebhookRepository(newRepo),
		Sender:     models.WebhookUser{Did: actor.String()},
	}
	payloadBytes, err := json.Marshal(payload)
	if err != nil {
		w.logger.Error("failed to marshal rename payload", "repo_did", newRepo.RepoDid, "err", err)
		return
	}

	userAgent := "Tangled-Hook/rename"
	for _, webhook := range webhooks {
		go w.sendWebhook(ctx, webhook, string(models.WebhookEventRepoRenamed), payload.Repository.FullName, userAgent, payloadBytes)
	}
}

func (w *Notifier) NewPull(ctx context.Context, pull *models.Pull) {
	w.pullRequestEvent(ctx, models.WebhookEventPullRequestCreated, "created", pull.OwnerDid, pull)
}

func (w *Notifier) ResubmitPull(ctx context.Context, pull *models.Pull) {
	w.pullRequestEvent(ctx, models.WebhookEventPullRequestResubmitted, "resubmitted", pull.OwnerDid, pull)
}

func (w *Notifier) NewPullState(ctx context.Context, actor syntax.DID, pull *models.Pull) {
	event, action, ok := pullStateEvent(pull.State)
	if !ok {
		return
	}
	w.pullRequestEvent(ctx, event, action, actor.String(), pull)
}

// pullStateEvent maps a pull's state to the webhook event announcing the
// transition into that state
func pullStateEvent(state models.PullState) (models.WebhookEvent, string, bool) {
	switch state {
	case models.PullMerged:
		return models.WebhookEventPullRequestMerged, "merged", true
	case models.PullClosed:
		return models.WebhookEventPullRequestClosed, "closed", true
	case models.PullOpen:
		return models.WebhookEventPullRequestReopened, "reopened", true
	default:
		return "", "", false
	}
}

func (w *Notifier) pullRequestEvent(ctx context.Context, event models.WebhookEvent, action, sender string, pull *models.Pull) {
	// pull request events originate from http handlers, whose context is
	// canceled as soon as the handler returns; detach so in-flight
	// deliveries are not cut short
	ctx = context.WithoutCancel(ctx)

	webhooks, err := w.activeWebhooksForEvent(string(pull.RepoDid), event)
	if err != nil {
		w.logger.Error("failed to get webhooks for repo", "repo_did", pull.RepoDid, "err", err)
		return
	}
	if len(webhooks) == 0 {
		return
	}

	repo, err := db.GetRepo(w.db, orm.FilterEq("repo_did", string(pull.RepoDid)))
	if err != nil {
		w.logger.Error("failed to get repo", "repo_did", pull.RepoDid, "err", err)
		return
	}

	payload := buildPullRequestPayload(action, repo, pull, sender, w.baseUrl)
	payloadBytes, err := json.Marshal(payload)
	if err != nil {
		w.logger.Error("failed to marshal pull request payload", "repo_did", pull.RepoDid, "err", err)
		return
	}

	userAgent := "Tangled-Hook/pull_request"
	for _, webhook := range webhooks {
		go w.sendWebhook(ctx, webhook, string(event), payload.Repository.FullName, userAgent, payloadBytes)
	}
}

func buildPullRequestPayload(action string, repo *models.Repo, pull *models.Pull, sender, baseUrl string) *models.WebhookPullRequestPayload {
	htmlUrl := fmt.Sprintf("%s/%s/%s/pulls/%d", baseUrl, repo.Did, repo.Slug(), pull.PullId)

	pullRequest := models.WebhookPullRequest{
		Number:       pull.PullId,
		Title:        pull.Title,
		Body:         pull.Body,
		State:        pull.State.String(),
		TargetBranch: pull.TargetBranch,
		Owner:        models.WebhookUser{Did: pull.OwnerDid},
		HtmlUrl:      htmlUrl,
		CreatedAt:    pull.Created.Format(time.RFC3339),
	}
	if len(pull.Submissions) > 0 {
		pullRequest.RoundNumber = pull.LastRoundNumber()
		pullRequest.PatchUrl = fmt.Sprintf("%s/round/%d.patch", htmlUrl, pull.LastRoundNumber())
	}
	if pull.PullSource != nil {
		source := &models.WebhookPullRequestSource{
			Branch: pull.PullSource.Branch,
		}
		if len(pull.Submissions) > 0 {
			source.Sha = pull.LatestSha()
		}
		if pull.IsForkBased() {
			source.Repo = pull.PullSource.RepoDid.String()
		}
		pullRequest.Source = source
	}

	return &models.WebhookPullRequestPayload{
		Action:      action,
		PullRequest: pullRequest,
		Repository:  buildWebhookRepository(repo),
		Sender:      models.WebhookUser{Did: sender},
	}
}

// Redeliver re-sends a stored delivery via the live send-and-record path,
// signing with the webhook's current secret.
func (w *Notifier) Redeliver(ctx context.Context, webhook models.Webhook, prev models.WebhookDelivery) {
	// recover the repo full name (X-Tangled-Repo header) from the stored payload
	var meta struct {
		Repository struct {
			FullName string `json:"full_name"`
		} `json:"repository"`
	}
	_ = json.Unmarshal([]byte(prev.RequestBody), &meta)

	w.sendWebhook(ctx, webhook, prev.Event, meta.Repository.FullName, "Tangled-Hook/retry", []byte(prev.RequestBody))
}

func (w *Notifier) activeWebhooksForEvent(repoDid string, event models.WebhookEvent) ([]models.Webhook, error) {
	webhooks, err := db.GetActiveWebhooksForRepo(w.db, repoDid)
	if err != nil {
		return nil, err
	}
	var matching []models.Webhook
	for _, webhook := range webhooks {
		if webhook.HasEvent(event) {
			matching = append(matching, webhook)
		}
	}
	return matching, nil
}

func buildWebhookRepository(repo *models.Repo) models.WebhookRepository {
	repository := models.WebhookRepository{
		Name:        repo.Name,
		FullName:    fmt.Sprintf("%s/%s", repo.Did, repo.Rkey),
		Description: repo.Description,
		Fork:        repo.Source != "",
		HtmlUrl:     fmt.Sprintf("https://%s/%s/%s", repo.Knot, repo.Did, repo.Rkey),
		CloneUrl:    fmt.Sprintf("https://%s/%s/%s", repo.Knot, repo.Did, repo.Rkey),
		SshUrl:      fmt.Sprintf("ssh://git@%s/%s/%s", repo.Knot, repo.Did, repo.Rkey),
		CreatedAt:   repo.Created.Format(time.RFC3339),
		UpdatedAt:   repo.Created.Format(time.RFC3339),
		Owner: models.WebhookUser{
			Did: repo.Did,
		},
	}
	if repo.Website != "" {
		repository.Website = repo.Website
	}
	if repo.RepoStats != nil {
		repository.StarsCount = repo.RepoStats.StarCount
		repository.OpenIssues = repo.RepoStats.IssueCount.Open
	}
	return repository
}

func (w *Notifier) buildPushPayload(repo *models.Repo, ref, oldSha, newSha, committerDid string) *models.WebhookPayload {
	pusher := committerDid
	if committerDid == "" {
		pusher = repo.Did
	}
	return &models.WebhookPayload{
		Ref:        ref,
		Before:     oldSha,
		After:      newSha,
		Repository: buildWebhookRepository(repo),
		Pusher: models.WebhookUser{
			Did: pusher,
		},
	}
}

func (w *Notifier) sendWebhook(ctx context.Context, webhook models.Webhook, event, repoFullName, userAgent string, payloadBytes []byte) {
	deliveryId := uuid.New().String()

	req, err := http.NewRequestWithContext(ctx, "POST", webhook.Url, bytes.NewReader(payloadBytes))
	if err != nil {
		w.logger.Error("failed to create webhook request", "webhook_id", webhook.Id, "err", err)
		return
	}

	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("X-Tangled-Event", event)
	req.Header.Set("X-Tangled-Hook-ID", fmt.Sprintf("%d", webhook.Id))
	req.Header.Set("X-Tangled-Delivery", deliveryId)
	req.Header.Set("X-Tangled-Repo", repoFullName)

	if webhook.Secret != "" {
		signature := w.computeSignature(payloadBytes, webhook.Secret)
		req.Header.Set("X-Tangled-Signature-256", "sha256="+signature)
	}

	delivery := &models.WebhookDelivery{
		WebhookId:   webhook.Id,
		Event:       event,
		DeliveryId:  deliveryId,
		Url:         webhook.Url,
		RequestBody: string(payloadBytes),
	}

	retryOpts := []retry.Option{
		retry.Attempts(3),
		retry.Delay(1 * time.Second),
		retry.MaxDelay(10 * time.Second),
		retry.DelayType(retry.BackOffDelay),
		retry.LastErrorOnly(true),
		retry.OnRetry(func(n uint, err error) {
			w.logger.Info("retrying webhook delivery",
				"webhook_id", webhook.Id,
				"attempt", n+1,
				"err", err)
		}),
		retry.Context(ctx),
		retry.RetryIf(func(err error) bool {
			return err != nil
		}),
	}

	var resp *http.Response
	err = retry.Do(func() error {
		var err error
		resp, err = w.client.Do(req)
		if err != nil {
			return err
		}
		if resp.StatusCode >= 500 {
			defer resp.Body.Close()
			return fmt.Errorf("server error: %d", resp.StatusCode)
		}
		return nil
	}, retryOpts...)

	if err != nil {
		w.logger.Error("webhook request failed after retries", "webhook_id", webhook.Id, "err", err)
		delivery.Success = false
		delivery.ResponseBody = err.Error()
	} else {
		defer resp.Body.Close()

		delivery.ResponseCode = resp.StatusCode
		delivery.Success = resp.StatusCode >= 200 && resp.StatusCode < 300

		bodyBytes, err := io.ReadAll(io.LimitReader(resp.Body, 10*1024))
		if err != nil {
			w.logger.Warn("failed to read webhook response body", "webhook_id", webhook.Id, "err", err)
		} else {
			delivery.ResponseBody = string(bodyBytes)
		}

		if !delivery.Success {
			w.logger.Warn("webhook delivery failed",
				"webhook_id", webhook.Id,
				"status", resp.StatusCode,
				"url", webhook.Url)
		} else {
			w.logger.Info("webhook delivered successfully",
				"webhook_id", webhook.Id,
				"url", webhook.Url,
				"delivery_id", deliveryId)
		}
	}

	if err := db.AddWebhookDelivery(w.db, delivery); err != nil {
		w.logger.Error("failed to record webhook delivery", "webhook_id", webhook.Id, "err", err)
	}
}

func (w *Notifier) computeSignature(payload []byte, secret string) string {
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write(payload)
	return hex.EncodeToString(mac.Sum(nil))
}
