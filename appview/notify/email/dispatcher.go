package email

import (
	"bytes"
	"context"
	"fmt"
	"html/template"
	"log/slog"
	"strings"
	gotemplate "text/template"
	"time"

	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/email"
	"tangled.org/core/appview/models"
	"tangled.org/core/idresolver"
)

const digestTextTmpl = `Hi {{.RecipientHandle}},

You have {{.Count}} new notification(s) on Tangled:

{{range .Groups}}- {{wrap 70 "  " .Header}}{{if .EntityRef}}
  {{wrap 70 "  " .EntityRef}}{{end}}
  {{.URL}}
{{end}}{{if .HasMore}}
View more notifications: {{.NotificationsURL}}
{{end}}---
Manage notifications: {{.SettingsURL}}
`

const digestHTMLTmpl = `<!DOCTYPE html>
<html dir="ltr" lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Tangled notifications</title>
</head>
<body style="background-color:#ffffff;padding:0;font-family:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;">
<table border="0" width="100%" cellpadding="0" cellspacing="0" role="presentation" align="center">
  <tbody>
    <tr>
      <td style="background-color:#ffffff;padding:16px;color:#111827;font-size:16px;font-weight:400;">
        <table align="center" border="0" cellpadding="0" cellspacing="0" role="presentation" style="max-width:600px;width:100%;table-layout:fixed;color:#111827;background-color:#ffffff;margin-left:auto;margin-right:auto;">
          <tbody>
            <tr>
              <td>
                <div style="padding-bottom:16px;">
                  <img src="{{.AssetsURL}}dolly.png" width="40" height="40" alt="Tangled" style="display:block;border:0;">
                </div>
                <p style="font-size:16px;padding:16px 0;">Hi {{.RecipientHandle}},</p>
                <p style="font-size:16px;padding:0 0 16px 0;margin:0;">
                  You have {{.Count}} new notification{{if gt .Count 1}}s{{end}}:
                </p>
                <table width="100%" cellpadding="0" cellspacing="0" role="presentation">
                  {{range .Groups}}
                  <tr>
                    <td style="padding:12px 0;border-bottom:1px solid #eaeaea;">
                      <a href="{{.URL}}" style="text-decoration:none;color:inherit;display:block;">
                        <table cellpadding="0" cellspacing="0" width="100%" role="presentation">
                          <tr>
                            <td style="width:20px;vertical-align:top;padding-top:1px;">
                              <img src="{{.IconURL}}" width="16" height="16" alt="" style="display:block;border:0;">
                            </td>
                            <td style="padding-left:8px;">
                              <p style="margin:0 0 3px 0;font-size:14px;color:#374151;">{{.HeaderHTML}}</p>
                              {{if .EntityRef}}<p style="margin:0;font-size:13px;color:#6b7280;">{{.EntityRef}}</p>{{end}}
                            </td>
                          </tr>
                        </table>
                      </a>
                    </td>
                  </tr>
                  {{end}}
                </table>
                {{if .HasMore}}
                <p style="font-size:14px;padding:16px 0 0 0;margin:0;">
                  <a href="{{.NotificationsURL}}" style="color:#111827;text-decoration:underline;font-weight:400;">View more notifications</a>
                </p>
                {{end}}
                <table align="center" width="100%" border="0" cellpadding="0" cellspacing="0" role="presentation">
                  <tbody>
                    <tr>
                      <td>
                        <p style="font-size:16px;padding:32px 0 16px 0;text-align:center;">
                          <a href="{{.SettingsURL}}" style="color:#111827;text-decoration:underline;font-weight:400;">Manage notification settings</a>
                        </p>
                        <p style="font-size:16px;text-align:center;color:#6b7280;">Tangled Labs Oy. &copy; 2026 All rights reserved.</p>
                        <p style="font-size:16px;text-align:center;">
                          <a href="https://tangled.org" style="color:#111827;text-decoration:underline;font-weight:400;">tangled.org</a>
                          &nbsp;&middot;&nbsp;
                          <a href="https://bsky.app/profile/tangled.org" style="color:#111827;text-decoration:underline;font-weight:400;">Bluesky</a>
                          &nbsp;&middot;&nbsp;
                          <a href="https://x.com/tangled_org" style="color:#111827;text-decoration:underline;font-weight:400;">X</a>
                          &nbsp;&middot;&nbsp;
                          <a href="https://linkedin.com/in/tangled" style="color:#111827;text-decoration:underline;font-weight:400;">LinkedIn</a>
                        </p>
                      </td>
                    </tr>
                  </tbody>
                </table>
              </td>
            </tr>
          </tbody>
        </table>
      </td>
    </tr>
  </tbody>
</table>
</body>
</html>`

type digestGroup struct {
	IconURL    string
	Header     string
	HeaderHTML template.HTML
	EntityRef  string
	URL        string
}

func notifHeader(n *models.NotificationWithEntity, actor, repo string) string {
	switch n.Type {
	case models.NotificationTypeIssueCreated:
		return actor + " opened an issue on " + repo
	case models.NotificationTypeIssueCommented:
		return actor + " commented on an issue on " + repo
	case models.NotificationTypeIssueClosed:
		return actor + " closed an issue on " + repo
	case models.NotificationTypeIssueReopen:
		return actor + " reopened an issue on " + repo
	case models.NotificationTypePullCreated:
		return actor + " created a PR on " + repo
	case models.NotificationTypePullCommented:
		return actor + " commented on a PR on " + repo
	case models.NotificationTypePullMerged:
		return actor + " merged a PR on " + repo
	case models.NotificationTypePullClosed:
		return actor + " closed a PR on " + repo
	case models.NotificationTypePullReopen:
		return actor + " reopened a PR on " + repo
	case models.NotificationTypeUserMentioned:
		if n.Issue != nil {
			return actor + " mentioned you on an issue in " + repo
		} else if n.Pull != nil {
			return actor + " mentioned you on a pull request in " + repo
		}
		return actor + " mentioned you in " + repo
	case models.NotificationTypeIssueAssigned:
		return actor + " assigned you to an issue on " + repo
	case models.NotificationTypeIssueUnassigned:
		return actor + " unassigned you from an issue on " + repo
	case models.NotificationTypePullAssigned:
		return actor + " assigned you to a PR on " + repo
	case models.NotificationTypePullUnassigned:
		return actor + " unassigned you from a PR on " + repo
	default:
		return actor + " updated " + repo
	}
}

func notifEntityRef(n *models.NotificationWithEntity) string {
	if n.Issue != nil {
		return fmt.Sprintf("#%d %s", n.Issue.IssueId, n.Issue.Title)
	}
	if n.Pull != nil {
		return fmt.Sprintf("#%d %s", n.Pull.PullId, n.Pull.Title)
	}
	return ""
}

func wordwrap(width int, indent, text string) string {
	words := strings.Fields(text)
	if len(words) == 0 {
		return text
	}
	var b strings.Builder
	col := 0
	for i, w := range words {
		if i == 0 {
			b.WriteString(w)
			col = len(w)
			continue
		}
		if col+1+len(w) > width {
			b.WriteString("\n" + indent)
			b.WriteString(w)
			col = len(indent) + len(w)
		} else {
			b.WriteByte(' ')
			b.WriteString(w)
			col += 1 + len(w)
		}
	}
	return b.String()
}

// digestMaxGroups caps how many notifications are itemized in a digest email;
// beyond this a "View more" link points to the notifications page.
const digestMaxGroups = 10

type digestData struct {
	RecipientHandle  string
	Count            int
	Groups           []digestGroup
	HasMore          bool
	NotificationsURL string
	SettingsURL      string
	AssetsURL        string
}

// Dispatcher polls the notifications table and sends digest emails.
type Dispatcher struct {
	db        *db.DB
	resend    config.ResendConfig
	baseURL   string
	assetsURL string
	resolver  *idresolver.Resolver
	logger    *slog.Logger
	batchWait time.Duration
	interval  time.Duration

	textTmpl *gotemplate.Template
	htmlTmpl *template.Template
}

func NewDispatcher(
	database *db.DB,
	resend config.ResendConfig,
	baseURL string,
	resolver *idresolver.Resolver,
	logger *slog.Logger,
	dev bool,
) *Dispatcher {
	batchWait := 10 * time.Minute
	interval := 5 * time.Minute
	if dev {
		batchWait = 30 * time.Second
		interval = 15 * time.Second
	}
	return &Dispatcher{
		db:        database,
		resend:    resend,
		baseURL:   strings.TrimRight(baseURL, "/"),
		assetsURL: strings.TrimRight(resend.AssetsURL, "/") + "/",
		resolver:  resolver,
		logger:    logger,
		batchWait: batchWait,
		interval:  interval,
		textTmpl:  gotemplate.Must(gotemplate.New("digest-text").Funcs(gotemplate.FuncMap{"wrap": wordwrap}).Parse(digestTextTmpl)),
		htmlTmpl:  template.Must(template.New("digest-html").Parse(digestHTMLTmpl)),
	}
}

// Start runs the dispatcher ticker loop until ctx is cancelled.
func (d *Dispatcher) Start(ctx context.Context) {
	d.logger.Info("email dispatcher started", "interval", d.interval, "batchWait", d.batchWait)
	ticker := time.NewTicker(d.interval)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			d.dispatch(ctx)
		case <-ctx.Done():
			d.logger.Info("email dispatcher stopped")
			return
		}
	}
}

func (d *Dispatcher) dispatch(ctx context.Context) {
	cutoff := time.Now().Add(-d.batchWait)

	recipients, err := db.GetPendingEmailDigestRecipients(d.db, cutoff)
	if err != nil {
		d.logger.Error("email dispatcher: failed to get recipients", "err", err)
		return
	}

	d.logger.Debug("email dispatcher: processing recipients", "count", len(recipients))

	for _, recipientDid := range recipients {
		if err := d.sendDigest(ctx, recipientDid, cutoff); err != nil {
			d.logger.Error("email dispatcher: failed to send digest", "did", recipientDid, "err", err)
		}
	}
}

func (d *Dispatcher) sendDigest(ctx context.Context, recipientDid string, cutoff time.Time) error {
	em, err := db.GetPrimaryEmail(d.db, recipientDid)
	if err != nil || !em.Verified {
		return nil
	}

	notifs, err := db.GetPendingNotificationsForEmailDigest(d.db, recipientDid, cutoff)
	if err != nil {
		return fmt.Errorf("get pending notifications: %w", err)
	}
	if len(notifs) == 0 {
		return nil
	}

	handle := recipientDid
	if id, err := d.resolver.ResolveIdent(ctx, recipientDid); err == nil && !id.Handle.IsInvalidHandle() {
		handle = id.Handle.String()
	}

	subject, text, html, err := d.renderDigest(ctx, handle, notifs)
	if err != nil {
		return fmt.Errorf("render digest: %w", err)
	}

	// collect IDs before sending so new notifications created during sending
	// aren't accidentally marked as emailed.
	ids := make([]int64, len(notifs))
	for i, n := range notifs {
		ids[i] = n.ID
	}

	if err := email.SendEmail(email.Email{
		APIKey:  d.resend.ApiKey,
		From:    "Tangled <" + d.resend.SentFrom + ">",
		To:      em.Address,
		Subject: subject,
		Text:    text,
		Html:    html,
	}); err != nil {
		return fmt.Errorf("send email: %w", err)
	}

	d.logger.Info("email dispatcher: digest sent", "did", recipientDid, "notifications", len(notifs))

	if err := db.MarkNotificationsEmailed(d.db, ids); err != nil {
		d.logger.Error("email dispatcher: failed to mark notifications emailed", "did", recipientDid, "err", err)
	}

	return nil
}

func (d *Dispatcher) renderDigest(ctx context.Context, recipientHandle string, notifs []*models.NotificationWithEntity) (subject, text, html string, err error) {
	count := len(notifs)

	shown := notifs
	if len(shown) > digestMaxGroups {
		shown = shown[:digestMaxGroups]
	}

	groups := make([]digestGroup, 0, len(shown))

	for _, n := range shown {
		actorHandle := n.ActorDid
		if id, err2 := d.resolver.ResolveIdent(ctx, n.ActorDid); err2 == nil && !id.Handle.IsInvalidHandle() {
			actorHandle = id.Handle.String()
		}

		repoStr := ""
		if n.Repo != nil {
			repoHandle := n.Repo.Did
			if id, err2 := d.resolver.ResolveIdent(ctx, n.Repo.Did); err2 == nil && !id.Handle.IsInvalidHandle() {
				repoHandle = id.Handle.String()
			}
			repoStr = repoHandle + "/" + n.Repo.Slug()
		}

		header := notifHeader(n, actorHandle, repoStr)
		headerHTML := template.HTML(strings.Replace(header, actorHandle, "<strong>"+actorHandle+"</strong>", 1))
		groups = append(groups, digestGroup{
			IconURL:    d.assetsURL + n.Icon() + ".png",
			Header:     header,
			HeaderHTML: headerHTML,
			EntityRef:  notifEntityRef(n),
			URL:        d.baseURL + n.URL(d.resolver),
		})
	}

	data := digestData{
		RecipientHandle:  recipientHandle,
		Count:            count,
		Groups:           groups,
		HasMore:          count > digestMaxGroups,
		NotificationsURL: d.baseURL + "/notifications",
		SettingsURL:      d.baseURL + "/settings/notifications",
		AssetsURL:        d.assetsURL,
	}

	if count > digestMaxGroups {
		subject = fmt.Sprintf("[%s] %d+ notifications", recipientHandle, digestMaxGroups)
	} else {
		subject = fmt.Sprintf("[%s] %d notification(s)", recipientHandle, count)
	}

	var textBuf bytes.Buffer
	if err = d.textTmpl.Execute(&textBuf, data); err != nil {
		return
	}
	text = textBuf.String()

	var htmlBuf bytes.Buffer
	if err = d.htmlTmpl.Execute(&htmlBuf, data); err != nil {
		return
	}
	html = htmlBuf.String()

	return
}
