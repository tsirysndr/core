package db

import (
	"database/sql"
	"fmt"
	"time"
)

// Newsletter preference status values. Both states hide the signup widget;
// they're distinguished so we can later reconcile with Resend or re-prompt
// dismissed-but-not-subscribed users if we ever want to.
const (
	NewsletterStatusSubscribed = "subscribed"
	NewsletterStatusDismissed  = "dismissed"
)

type NewsletterPref struct {
	ID        int64
	UserDid   string
	Status    string
	Email     string
	UpdatedAt time.Time
}

// GetNewsletterPref returns the newsletter preference row for a user, or nil
// when no row exists (the caller should treat nil as "show the widget").
func GetNewsletterPref(e Execer, did string) (*NewsletterPref, error) {
	var (
		pref      NewsletterPref
		email     sql.NullString
		updatedAt string
	)

	err := e.QueryRow(
		`select id, user_did, status, email, updated_at
		 from newsletter_preferences
		 where user_did = ?`,
		did,
	).Scan(&pref.ID, &pref.UserDid, &pref.Status, &email, &updatedAt)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}

	if email.Valid {
		pref.Email = email.String
	}
	if t, perr := time.Parse(time.RFC3339, updatedAt); perr == nil {
		pref.UpdatedAt = t
	}

	return &pref, nil
}

// UpsertNewsletterPref writes or replaces a user's newsletter preference and
// refreshes updated_at. Passing an empty email is fine — the column is
// nullable and is only meaningful for subscribed rows.
func UpsertNewsletterPref(e Execer, did, status, email string) error {
	if status != NewsletterStatusSubscribed && status != NewsletterStatusDismissed {
		return fmt.Errorf("invalid newsletter status %q", status)
	}

	var emailArg any
	if email != "" {
		emailArg = email
	} else {
		emailArg = nil
	}

	_, err := e.Exec(
		`insert into newsletter_preferences (user_did, status, email, updated_at)
		 values (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		 on conflict(user_did) do update set
		     status = excluded.status,
		     email = coalesce(excluded.email, newsletter_preferences.email),
		     updated_at = excluded.updated_at`,
		did,
		status,
		emailArg,
	)
	if err != nil {
		return fmt.Errorf("upsert newsletter pref: %w", err)
	}
	return nil
}
