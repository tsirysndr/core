package xrpc

import (
	"context"
	"encoding/json"
	"net/http"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/hostutil"
	xrpcerr "tangled.org/core/xrpc/errors"
)

// resolveOwnedRepo loads the repo by its DID and checks the actor owns it
func (x *Xrpc) resolveOwnedRepo(r *http.Request, repoDid string) (*models.Repo, *xrpcerr.XrpcError, int) {
	did, ok := actorDid(r)
	if !ok {
		e := xrpcerr.MissingActorDidError
		return nil, &e, http.StatusForbidden
	}

	repo, err := db.GetRepoByDid(x.DB, repoDid)
	if err != nil {
		e := notFoundError("repo not found")
		return nil, &e, http.StatusNotFound
	}

	if repo.Did != did {
		e := xrpcerr.AccessControlError(did)
		return nil, &e, http.StatusForbidden
	}

	return repo, nil, http.StatusOK
}

func (x *Xrpc) WebhookList(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookList")

	repo, xerr, status := x.resolveOwnedRepo(r, r.URL.Query().Get("repoDid"))
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	webhooks, err := db.GetWebhooksForRepo(x.DB, string(repo.RepoDid))
	if err != nil {
		l.Error("failed to get webhooks", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	items := make([]*tangled.TempRepoListWebhooks_Webhook, 0, len(webhooks))
	for i := range webhooks {
		wh := &webhooks[i]
		updated := wh.UpdatedAt.UTC().Format(timeFormat)
		items = append(items, &tangled.TempRepoListWebhooks_Webhook{
			Id:        wh.Id,
			Url:       wh.Url,
			Active:    wh.Active,
			Events:    wh.Events,
			CreatedAt: wh.CreatedAt.UTC().Format(timeFormat),
			UpdatedAt: &updated,
		})
	}

	x.writeJSON(w, &tangled.TempRepoListWebhooks_Output{Webhooks: items})
}

func (x *Xrpc) WebhookCreate(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookCreate")

	var input tangled.TempRepoCreateWebhook_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	url := strings.TrimSpace(input.Url)
	if err := hostutil.ValidateExternalURL(url, x.Config.Core.Dev); err != nil {
		writeError(w, badRequestError(err.Error()), http.StatusBadRequest)
		return
	}
	if len(input.Events) == 0 {
		writeError(w, xrpcErrorTag("NoEventsSelected", "at least one event must be specified"), http.StatusBadRequest)
		return
	}

	active := true
	if input.Active != nil {
		active = *input.Active
	}
	secret := ""
	if input.Secret != nil {
		secret = strings.TrimSpace(*input.Secret)
	}

	webhook := &models.Webhook{
		RepoDid: syntax.DID(repo.RepoDid),
		Url:     url,
		Secret:  secret,
		Active:  active,
		Events:  input.Events,
	}

	tx, err := x.DB.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	defer tx.Rollback()

	if err := db.AddWebhook(tx, webhook); err != nil {
		l.Error("failed to add webhook", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	x.writeJSON(w, &tangled.TempRepoCreateWebhook_Output{Id: webhook.Id})
}

func (x *Xrpc) WebhookUpdate(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookUpdate")

	var input tangled.TempRepoUpdateWebhook_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	webhook, err := db.GetWebhook(x.DB, input.Id)
	if err != nil || string(webhook.RepoDid) != repo.RepoDid {
		writeError(w, xrpcErrorTag("WebhookNotFound", "webhook not found"), http.StatusNotFound)
		return
	}

	if input.Url != nil {
		url := strings.TrimSpace(*input.Url)
		if url != "" {
			if err := hostutil.ValidateExternalURL(url, x.Config.Core.Dev); err != nil {
				writeError(w, badRequestError(err.Error()), http.StatusBadRequest)
				return
			}
			webhook.Url = url
		}
	}
	if input.Secret != nil {
		webhook.Secret = strings.TrimSpace(*input.Secret)
	}
	if input.Active != nil {
		webhook.Active = *input.Active
	}
	if len(input.Events) > 0 {
		webhook.Events = input.Events
	}

	tx, err := x.DB.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	defer tx.Rollback()

	if err := db.UpdateWebhook(tx, webhook); err != nil {
		l.Error("failed to update webhook", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) WebhookDelete(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookDelete")

	var input tangled.TempRepoDeleteWebhook_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	webhook, err := db.GetWebhook(x.DB, input.Id)
	if err != nil || string(webhook.RepoDid) != repo.RepoDid {
		writeError(w, xrpcErrorTag("WebhookNotFound", "webhook not found"), http.StatusNotFound)
		return
	}

	tx, err := x.DB.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	defer tx.Rollback()

	if err := db.DeleteWebhook(tx, input.Id); err != nil {
		l.Error("failed to delete webhook", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) WebhookToggle(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookToggle")

	var input tangled.TempRepoToggleWebhook_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	webhook, err := db.GetWebhook(x.DB, input.Id)
	if err != nil || string(webhook.RepoDid) != repo.RepoDid {
		writeError(w, xrpcErrorTag("WebhookNotFound", "webhook not found"), http.StatusNotFound)
		return
	}

	webhook.Active = !webhook.Active

	tx, err := x.DB.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	defer tx.Rollback()

	if err := db.UpdateWebhook(tx, webhook); err != nil {
		l.Error("failed to toggle webhook", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	x.writeJSON(w, &tangled.TempRepoToggleWebhook_Output{Active: webhook.Active})
}

func (x *Xrpc) WebhookListDeliveries(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "WebhookListDeliveries")

	q := r.URL.Query()
	repo, xerr, status := x.resolveOwnedRepo(r, q.Get("repoDid"))
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	id, err := strconv.ParseInt(q.Get("id"), 10, 64)
	if err != nil {
		writeError(w, badRequestError("invalid webhook id"), http.StatusBadRequest)
		return
	}

	webhook, err := db.GetWebhook(x.DB, id)
	if err != nil || string(webhook.RepoDid) != repo.RepoDid {
		writeError(w, xrpcErrorTag("WebhookNotFound", "webhook not found"), http.StatusNotFound)
		return
	}

	limit := 100
	if s := q.Get("limit"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n > 0 && n <= 100 {
			limit = n
		}
	}

	deliveries, err := db.GetWebhookDeliveries(x.DB, webhook.Id, limit)
	if err != nil {
		l.Error("failed to get webhook deliveries", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	items := make([]*tangled.TempRepoListWebhookDeliveries_Delivery, 0, len(deliveries))
	for i := range deliveries {
		d := &deliveries[i]
		item := &tangled.TempRepoListWebhookDeliveries_Delivery{
			Id:         d.Id,
			DeliveryId: d.DeliveryId,
			Event:      d.Event,
			Url:        d.Url,
			Success:    d.Success,
			CreatedAt:  d.CreatedAt.UTC().Format(timeFormat),
		}
		if d.RequestBody != "" {
			rb := d.RequestBody
			item.RequestBody = &rb
		}
		if d.ResponseBody != "" {
			rb := d.ResponseBody
			item.ResponseBody = &rb
		}
		if d.ResponseCode != 0 {
			rc := int64(d.ResponseCode)
			item.ResponseCode = &rc
		}
		items = append(items, item)
	}

	x.writeJSON(w, &tangled.TempRepoListWebhookDeliveries_Output{Deliveries: items})
}

func (x *Xrpc) WebhookRetryDelivery(w http.ResponseWriter, r *http.Request) {
	var input tangled.TempRepoRetryWebhookDelivery_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	webhook, err := db.GetWebhook(x.DB, input.WebhookId)
	if err != nil || string(webhook.RepoDid) != repo.RepoDid {
		writeError(w, xrpcErrorTag("WebhookNotFound", "webhook not found"), http.StatusNotFound)
		return
	}

	delivery, err := db.GetWebhookDelivery(x.DB, input.DeliveryId)
	if err != nil || delivery.WebhookId != webhook.Id {
		writeError(w, xrpcErrorTag("DeliveryNotFound", "delivery not found"), http.StatusNotFound)
		return
	}

	// re-dispatch async; the new attempt is recorded as its own delivery
	go x.Webhooks.Redeliver(context.Background(), *webhook, *delivery)

	w.WriteHeader(http.StatusOK)
}
