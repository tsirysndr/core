package repo

import (
	"net/http"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
)

// webhookEventFields maps event checkbox form fields to webhook events
var webhookEventFields = []struct {
	field string
	event models.WebhookEvent
}{
	{"event_push", models.WebhookEventPush},
	{"event_repo_renamed", models.WebhookEventRepoRenamed},
	{"event_pull_request_created", models.WebhookEventPullRequestCreated},
	{"event_pull_request_resubmitted", models.WebhookEventPullRequestResubmitted},
	{"event_pull_request_merged", models.WebhookEventPullRequestMerged},
	{"event_pull_request_closed", models.WebhookEventPullRequestClosed},
	{"event_pull_request_reopened", models.WebhookEventPullRequestReopened},
}

func webhookEventsFromForm(r *http.Request) []string {
	events := []string{}
	for _, ef := range webhookEventFields {
		if r.FormValue(ef.field) == "on" {
			events = append(events, string(ef.event))
		}
	}
	return events
}

// Webhooks displays the webhooks settings page
func (rp *Repo) Webhooks(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "Webhooks")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	user := rp.oauth.GetMultiAccountUser(r)

	webhooks, err := db.GetWebhooksForRepo(rp.db, f.RepoDid)
	if err != nil {
		l.Error("failed to get webhooks", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to load webhooks")
		return
	}

	// fetch recent deliveries for each webhook
	deliveriesMap := make(map[int64][]models.WebhookDelivery)
	for _, webhook := range webhooks {
		deliveries, err := db.GetWebhookDeliveries(rp.db, webhook.Id, 4)
		if err != nil {
			l.Error("failed to get webhook deliveries", "webhook_id", webhook.Id, "err", err)
			// continue even if we can't get deliveries for one webhook
			continue
		}
		deliveriesMap[webhook.Id] = deliveries
	}

	rp.pages.RepoWebhooksSettings(w, pages.RepoWebhooksSettingsParams{
		BaseParams:        pages.BaseParamsFromContext(r.Context()),
		RepoInfo:          rp.repoResolver.GetRepoInfo(r, user),
		Webhooks:          webhooks,
		WebhookDeliveries: deliveriesMap,
	})
}

// AddWebhook creates a new webhook
func (rp *Repo) AddWebhook(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "AddWebhook")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	url := strings.TrimSpace(r.FormValue("url"))
	if url == "" {
		rp.pages.Notice(w, "webhooks-error", "Webhook URL is required")
		return
	}

	if !strings.HasPrefix(url, "http://") && !strings.HasPrefix(url, "https://") {
		rp.pages.Notice(w, "webhooks-error", "Webhook URL must start with http:// or https://")
		return
	}

	secret := strings.TrimSpace(r.FormValue("secret"))
	// if secret is empty, we don't sign

	active := r.FormValue("active") == "on"

	events := webhookEventsFromForm(r)
	if len(events) == 0 {
		rp.pages.Notice(w, "webhooks-error", "At least one event must be enabled")
		return
	}

	webhook := &models.Webhook{
		RepoDid: syntax.DID(f.RepoDid),
		Url:     url,
		Secret:  secret,
		Active:  active,
		Events:  events,
	}

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to create webhook")
		return
	}
	defer tx.Rollback()

	if err := db.AddWebhook(tx, webhook); err != nil {
		l.Error("failed to add webhook", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to create webhook")
		return
	}

	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to create webhook")
		return
	}

	rp.pages.HxRefresh(w)
}

// UpdateWebhook updates an existing webhook
func (rp *Repo) UpdateWebhook(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "UpdateWebhook")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	idStr := chi.URLParam(r, "id")
	id, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		l.Error("invalid webhook id", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	webhook, err := db.GetWebhook(rp.db, id)
	if err != nil {
		l.Error("failed to get webhook", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Webhook not found")
		return
	}

	// Verify webhook belongs to this repo
	if string(webhook.RepoDid) != f.RepoDid {
		l.Error("webhook does not belong to repo", "webhook_repo", webhook.RepoDid, "current_repo", f.RepoDid)
		w.WriteHeader(http.StatusForbidden)
		return
	}

	url := strings.TrimSpace(r.FormValue("url"))
	if url != "" {
		if !strings.HasPrefix(url, "http://") && !strings.HasPrefix(url, "https://") {
			rp.pages.Notice(w, "webhooks-error", "Webhook URL must start with http:// or https://")
			return
		}
		webhook.Url = url
	}

	secret := strings.TrimSpace(r.FormValue("secret"))
	if secret != "" {
		webhook.Secret = secret
	}

	webhook.Active = r.FormValue("active") == "on"

	events := webhookEventsFromForm(r)
	if len(events) > 0 {
		webhook.Events = events
	}

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to update webhook")
		return
	}
	defer tx.Rollback()

	if err := db.UpdateWebhook(tx, webhook); err != nil {
		l.Error("failed to update webhook", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to update webhook")
		return
	}

	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to update webhook")
		return
	}

	rp.pages.HxRefresh(w)
}

// DeleteWebhook deletes a webhook
func (rp *Repo) DeleteWebhook(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DeleteWebhook")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	idStr := chi.URLParam(r, "id")
	id, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		l.Error("invalid webhook id", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	webhook, err := db.GetWebhook(rp.db, id)
	if err != nil {
		l.Error("failed to get webhook", "err", err)
		w.WriteHeader(http.StatusNotFound)
		return
	}

	// Verify webhook belongs to this repo
	if string(webhook.RepoDid) != f.RepoDid {
		l.Error("webhook does not belong to repo", "webhook_repo", webhook.RepoDid, "current_repo", f.RepoDid)
		w.WriteHeader(http.StatusForbidden)
		return
	}

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to delete webhook")
		return
	}
	defer tx.Rollback()

	if err := db.DeleteWebhook(tx, id); err != nil {
		l.Error("failed to delete webhook", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to delete webhook")
		return
	}

	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to delete webhook")
		return
	}

	rp.pages.HxRefresh(w)
}

// ToggleWebhook toggles the active state of a webhook
func (rp *Repo) ToggleWebhook(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "ToggleWebhook")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	idStr := chi.URLParam(r, "id")
	id, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		l.Error("invalid webhook id", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	webhook, err := db.GetWebhook(rp.db, id)
	if err != nil {
		l.Error("failed to get webhook", "err", err)
		w.WriteHeader(http.StatusNotFound)
		return
	}

	// Verify webhook belongs to this repo
	if string(webhook.RepoDid) != f.RepoDid {
		l.Error("webhook does not belong to repo", "webhook_repo", webhook.RepoDid, "current_repo", f.RepoDid)
		w.WriteHeader(http.StatusForbidden)
		return
	}

	// Toggle the active state
	webhook.Active = !webhook.Active

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to toggle webhook")
		return
	}
	defer tx.Rollback()

	if err := db.UpdateWebhook(tx, webhook); err != nil {
		l.Error("failed to update webhook", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to toggle webhook")
		return
	}

	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to toggle webhook")
		return
	}

	rp.pages.HxRefresh(w)
}

// WebhookDeliveries returns all deliveries for a webhook (for modal display)
func (rp *Repo) WebhookDeliveries(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "WebhookDeliveries")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	idStr := chi.URLParam(r, "id")
	id, err := strconv.ParseInt(idStr, 10, 64)
	if err != nil {
		l.Error("invalid webhook id", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	webhook, err := db.GetWebhook(rp.db, id)
	if err != nil {
		l.Error("failed to get webhook", "err", err)
		w.WriteHeader(http.StatusNotFound)
		return
	}

	// Verify webhook belongs to this repo
	if string(webhook.RepoDid) != f.RepoDid {
		l.Error("webhook does not belong to repo", "webhook_repo", webhook.RepoDid, "current_repo", f.RepoDid)
		w.WriteHeader(http.StatusForbidden)
		return
	}

	deliveries, err := db.GetWebhookDeliveries(rp.db, webhook.Id, 100)
	if err != nil {
		l.Error("failed to get webhook deliveries", "err", err)
		rp.pages.Notice(w, "webhooks-error", "Failed to load deliveries")
		return
	}

	user := rp.oauth.GetMultiAccountUser(r)

	rp.pages.WebhookDeliveriesList(w, pages.WebhookDeliveriesListParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Webhook:    webhook,
		Deliveries: deliveries,
	})
}
