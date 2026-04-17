package email

import (
	"fmt"
	"net"
	"net/mail"
	"strings"

	"github.com/resend/resend-go/v3"
)

type Email struct {
	From    string
	To      string
	Subject string
	Text    string
	Html    string
	APIKey  string
}

func SendEmail(email Email) error {
	client := resend.NewClient(email.APIKey)
	_, err := client.Emails.Send(&resend.SendEmailRequest{
		From:    email.From,
		To:      []string{email.To},
		Subject: email.Subject,
		Text:    email.Text,
		Html:    email.Html,
	})
	if err != nil {
		return fmt.Errorf("error sending email: %w", err)
	}
	return nil
}

// AddNewsletterContact adds an email address to the Resend newsletter segment.
func AddNewsletterContact(apiKey, segmentID, emailAddr string) error {
	client := resend.NewClient(apiKey)
	_, err := client.Contacts.Segments.Add(&resend.AddContactSegmentRequest{
		SegmentId: segmentID,
		Email:     emailAddr,
	})
	if err != nil {
		return fmt.Errorf("error adding contact to newsletter segment: %w", err)
	}
	return nil
}

func IsValidEmail(email string) bool {
	// Reject whitespace (ParseAddress normalizes it away)
	if strings.ContainsAny(email, " \t\n\r") {
		return false
	}

	// Use stdlib RFC 5322 parser
	addr, err := mail.ParseAddress(email)
	if err != nil {
		return false
	}

	// Split email into local and domain parts
	parts := strings.Split(addr.Address, "@")
	domain := parts[1]

	canonical := coalesceToCanonicalName(domain)
	mx, err := net.LookupMX(canonical)

	// Don't check err here; mx will only contain valid mx records, and we should
	// only fallback to an implicit mx if there are no mx records defined (whether
	// they are valid or not).
	if len(mx) != 0 {
		return true
	}

	if err != nil {
		// If the domain resolves to an address, assume it's an implicit mx.
		address, _ := net.LookupIP(canonical)
		if len(address) != 0 {
			return true
		}
	}

	return false
}

func coalesceToCanonicalName(domain string) string {
	canonical, err := net.LookupCNAME(domain)
	if err != nil {
		// net.LookupCNAME() returns an error if there is no cname record *and* no
		// a/aaaa records, but there may still be mx records.
		return domain
	}
	return canonical
}
