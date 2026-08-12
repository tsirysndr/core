package mailer

import (
	"fmt"
	"log/slog"

	appviewemail "tangled.org/core/appview/email"
	"tangled.org/core/deliberi/config"
)

type Sender struct {
	apiKey string
	from   string
	logger *slog.Logger
}

func New(resend config.ResendConfig, logger *slog.Logger) *Sender {
	return &Sender{apiKey: resend.ApiKey, from: resend.SentFrom, logger: logger}
}

// Send delivers one email. with no Resend key it prints to stdout instead of
// sending, and reports success so callers proceed normally in dev.
func (s *Sender) Send(to, subject, text, html string) error {
	from := "Tangled <" + s.from + ">"

	if s.apiKey == "" {
		s.logger.Info("resend api key unset; writing email to stdout", "to", to, "subject", subject)
		fmt.Printf("\n=== deliberi email (stdout, no resend key) ===\nfrom: %s\nto:   %s\nsubject: %s\n\n%s\n===============================================\n\n", from, to, subject, text)
		return nil
	}

	return appviewemail.SendEmail(appviewemail.Email{
		APIKey:  s.apiKey,
		From:    from,
		To:      to,
		Subject: subject,
		Text:    text,
		Html:    html,
	})
}
