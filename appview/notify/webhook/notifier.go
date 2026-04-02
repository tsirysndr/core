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
	"github.com/google/uuid"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/log"
)

type Notifier struct {
	notify.BaseNotifier
	db     *db.DB
	logger *slog.Logger
	client *http.Client
}

func NewNotifier(database *db.DB) *Notifier {
	return &Notifier{
		db:     database,
		logger: log.New("webhook-notifier"),
		client: &http.Client{
			Timeout: 30 * time.Second,
		},
	}
}

var _ notify.Notifier = &Notifier{}

func (w *Notifier) Push(ctx context.Context, repo *models.Repo, ref, oldSha, newSha, committerDid string) {
	webhooks, err := db.GetActiveWebhooksForRepo(w.db, repo.RepoAt())
	if err != nil {
		w.logger.Error("failed to get webhooks for repo", "repo", repo.RepoAt(), "err", err)
		return
	}

	var pushWebhooks []models.Webhook
	for _, webhook := range webhooks {
		if webhook.HasEvent(models.WebhookEventPush) {
			pushWebhooks = append(pushWebhooks, webhook)
		}
	}

	if len(pushWebhooks) == 0 {
		return
	}

	payload, err := w.buildPushPayload(repo, ref, oldSha, newSha, committerDid)
	if err != nil {
		w.logger.Error("failed to build push payload", "repo", repo.RepoAt(), "err", err)
		return
	}

	for _, webhook := range pushWebhooks {
		go w.sendWebhook(ctx, webhook, string(models.WebhookEventPush), payload)
	}
}

func (w *Notifier) buildPushPayload(repo *models.Repo, ref, oldSha, newSha, committerDid string) (*models.WebhookPayload, error) {
	owner := repo.Did

	pusher := committerDid
	if committerDid == "" {
		pusher = owner
	}

	repository := models.WebhookRepository{
		Name:        repo.Name,
		FullName:    fmt.Sprintf("%s/%s", repo.Did, repo.Name),
		Description: repo.Description,
		Fork:        repo.Source != "",
		HtmlUrl:     fmt.Sprintf("https://%s/%s/%s", repo.Knot, repo.Did, repo.Name),
		CloneUrl:    fmt.Sprintf("https://%s/%s/%s", repo.Knot, repo.Did, repo.Name),
		SshUrl:      fmt.Sprintf("ssh://git@%s/%s/%s", repo.Knot, repo.Did, repo.Name),
		CreatedAt:   repo.Created.Format(time.RFC3339),
		UpdatedAt:   repo.Created.Format(time.RFC3339),
		Owner: models.WebhookUser{
			Did: owner,
		},
	}

	if repo.Website != "" {
		repository.Website = repo.Website
	}
	if repo.RepoStats != nil {
		repository.StarsCount = repo.RepoStats.StarCount
		repository.OpenIssues = repo.RepoStats.IssueCount.Open
	}

	payload := &models.WebhookPayload{
		Ref:        ref,
		Before:     oldSha,
		After:      newSha,
		Repository: repository,
		Pusher: models.WebhookUser{
			Did: pusher,
		},
	}

	return payload, nil
}

func (w *Notifier) sendWebhook(ctx context.Context, webhook models.Webhook, event string, payload *models.WebhookPayload) {
	deliveryId := uuid.New().String()

	payloadBytes, err := json.Marshal(payload)
	if err != nil {
		w.logger.Error("failed to marshal webhook payload", "webhook_id", webhook.Id, "err", err)
		return
	}

	req, err := http.NewRequestWithContext(ctx, "POST", webhook.Url, bytes.NewReader(payloadBytes))
	if err != nil {
		w.logger.Error("failed to create webhook request", "webhook_id", webhook.Id, "err", err)
		return
	}

	shortSha := payload.After[:7]

	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", "Tangled-Hook/"+shortSha)
	req.Header.Set("X-Tangled-Event", event)
	req.Header.Set("X-Tangled-Hook-ID", fmt.Sprintf("%d", webhook.Id))
	req.Header.Set("X-Tangled-Delivery", deliveryId)
	req.Header.Set("X-Tangled-Repo", payload.Repository.FullName)

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
