package settings

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log"
	"log/slog"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/email"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/sites"
	"tangled.org/core/tid"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/gliderlabs/ssh"
	"github.com/google/uuid"
)

type Settings struct {
	Db       *db.DB
	OAuth    *oauth.OAuth
	Pages    *pages.Pages
	Config   *config.Config
	CfClient *cloudflare.Client
	Logger   *slog.Logger
}

func (s *Settings) Router() http.Handler {
	r := chi.NewRouter()

	r.Use(middleware.AuthMiddleware(s.OAuth))

	// settings pages
	r.Get("/", s.profileSettings)
	r.Get("/profile", s.profileSettings)

	r.Route("/keys", func(r chi.Router) {
		r.Get("/", s.keysSettings)
		r.Put("/", s.keys)
		r.Delete("/", s.keys)
	})

	r.Route("/emails", func(r chi.Router) {
		r.Get("/", s.emailsSettings)
		r.Put("/", s.emails)
		r.Delete("/", s.emails)
		r.Get("/verify", s.emailsVerify)
		r.Post("/verify/resend", s.emailsVerifyResend)
		r.Post("/primary", s.emailsPrimary)
	})

	r.Route("/notifications", func(r chi.Router) {
		r.Get("/", s.notificationsSettings)
		r.Put("/", s.updateNotificationPreferences)
	})

	r.Route("/sites", func(r chi.Router) {
		r.Get("/", s.sitesSettings)
		r.Put("/", s.claimSitesDomain)
		r.Delete("/", s.releaseSitesDomain)
	})

	return r
}

func (s *Settings) sitesSettings(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	did := s.OAuth.GetDid(r)

	claim, err := db.GetActiveDomainClaimForDid(s.Db, did)
	if err != nil {
		s.Logger.Error("failed to get domain claim", "err", err)
		claim = nil
	}

	// determine whether the active account has a tngl.sh handle, in which
	// case their sites domain is automatically their handle domain.
	pdsDomain := strings.TrimPrefix(s.Config.Pds.Host, "https://")
	pdsDomain = strings.TrimPrefix(pdsDomain, "http://")
	isTnglHandle := false
	for _, acc := range user.Accounts {
		if acc.Did == did && strings.HasSuffix(acc.Handle, "."+pdsDomain) {
			isTnglHandle = true
			break
		}
	}

	s.Pages.UserSiteSettings(w, pages.UserSiteSettingsParams{
		LoggedInUser: user,
		Claim:        claim,
		SitesDomain:  s.Config.Sites.Domain,
		IsTnglHandle: isTnglHandle,
	})
}

func (s *Settings) claimSitesDomain(w http.ResponseWriter, r *http.Request) {
	did := s.OAuth.GetDid(r)

	subdomain := strings.TrimSpace(r.FormValue("subdomain"))
	if subdomain == "" {
		s.Pages.Notice(w, "settings-sites-error", "Subdomain cannot be empty.")
		return
	}

	if len(subdomain) < 4 {
		s.Pages.Notice(w, "settings-sites-error", "Subdomain must be at least 4 characters long.")
		return
	}

	if !isValidSubdomain(subdomain) {
		s.Pages.Notice(w, "settings-sites-error", "Invalid subdomain. Use only lowercase letters, digits, and hyphens. Cannot start or end with a hyphen.")
		return
	}

	sitesDomain := s.Config.Sites.Domain

	if subdomain == sitesDomain {
		s.Pages.Notice(w, "settings-sites-error", fmt.Sprintf("You cannot claim the root domain %q.", sitesDomain))
		return
	}

	fullDomain := subdomain + "." + sitesDomain

	if err := db.ClaimDomain(s.Db, did, fullDomain); err != nil {
		switch {
		case errors.Is(err, db.ErrDomainTaken):
			s.Pages.Notice(w, "settings-sites-error", "That domain is already claimed by another user.")
		case errors.Is(err, db.ErrDomainCooldown):
			s.Pages.Notice(w, "settings-sites-error", "That domain was recently released and is in a 30-day cooldown period. Please try again later.")
		case errors.Is(err, db.ErrAlreadyClaimed):
			s.Pages.Notice(w, "settings-sites-error", "You already have a domain claimed. Release it before claiming a new one.")
		default:
			s.Logger.Error("claiming domain", "err", err)
			s.Pages.Notice(w, "settings-sites-error", "Unable to claim domain at this moment. Try again later.")
		}
		return
	}

	s.Pages.HxRefresh(w)
}

func (s *Settings) releaseSitesDomain(w http.ResponseWriter, r *http.Request) {
	did := s.OAuth.GetDid(r)
	domain := strings.TrimSpace(r.FormValue("domain"))

	if domain == "" {
		s.Pages.Notice(w, "settings-sites-error", "Domain cannot be empty.")
		return
	}

	pdsDomain := strings.TrimPrefix(s.Config.Pds.Host, "https://")
	pdsDomain = strings.TrimPrefix(pdsDomain, "http://")
	user := s.OAuth.GetMultiAccountUser(r)
	for _, acc := range user.Accounts {
		if acc.Did == did && strings.HasSuffix(acc.Handle, "."+pdsDomain) {
			if strings.HasSuffix(domain, "."+pdsDomain) {
				s.Pages.Notice(w, "settings-sites-error", "Your tngl.sh domain is tied to your handle and cannot be released here.")
				return
			}
		}
	}

	if err := db.ReleaseDomain(s.Db, did, domain); err != nil {
		s.Logger.Error("releasing domain", "err", err)
		s.Pages.Notice(w, "settings-sites-error", "Unable to release domain. Make sure it belongs to your account.")
		return
	}

	// Clean up all site data for this DID asynchronously.
	if s.CfClient.Enabled() {
		siteConfigs, err := db.GetRepoSiteConfigsForDid(s.Db, did)
		if err != nil {
			s.Logger.Error("releaseSitesDomain: fetching site configs for cleanup", "err", err)
		}

		if err := db.DeleteRepoSiteConfigsForDid(s.Db, did); err != nil {
			s.Logger.Error("releaseSitesDomain: deleting site configs from db", "err", err)
		}

		go func() {
			ctx := context.Background()

			// Delete each repo's R2 objects.
			for _, sc := range siteConfigs {
				if err := sites.Delete(ctx, s.CfClient, did, sc.RepoName); err != nil {
					s.Logger.Error("releaseSitesDomain: R2 delete failed", "did", did, "repo", sc.RepoName, "err", err)
				}
			}

			// Delete the single KV entry for the domain.
			if err := sites.DeleteAllDomainMappings(ctx, s.CfClient, domain); err != nil {
				s.Logger.Error("releaseSitesDomain: KV delete failed", "domain", domain, "err", err)
			}
		}()
	}

	s.Pages.HxLocation(w, "/settings/sites")
}

// isValidSubdomain checks that a subdomain label uses only lowercase letters,
// digits, and hyphens, and does not start or end with a hyphen.
func isValidSubdomain(s string) bool {
	if len(s) == 0 || len(s) > 63 {
		return false
	}
	if s[0] == '-' || s[len(s)-1] == '-' {
		return false
	}
	for _, c := range s {
		if !((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-') {
			return false
		}
	}
	return true
}

func (s *Settings) profileSettings(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)

	punchcardPreferences, err := db.GetPunchcardPreference(s.Db, user.Did())
	if err != nil {
		log.Printf("failed to get users punchcard preferences: %s", err)
	}

	s.Pages.UserProfileSettings(w, pages.UserProfileSettingsParams{
		LoggedInUser:        user,
		PunchcardPreference: punchcardPreferences,
	})
}

func (s *Settings) notificationsSettings(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	did := s.OAuth.GetDid(r)

	prefs, err := db.GetNotificationPreference(s.Db, did)
	if err != nil {
		s.Logger.Error("failed to get notification preferences", "err", err)
		s.Pages.Notice(w, "settings-notifications-error", "Unable to load notification preferences.")
		return
	}

	s.Pages.UserNotificationSettings(w, pages.UserNotificationSettingsParams{
		LoggedInUser: user,
		Preferences:  prefs,
	})
}

func (s *Settings) updateNotificationPreferences(w http.ResponseWriter, r *http.Request) {
	did := s.OAuth.GetDid(r)

	prefs := &models.NotificationPreferences{
		UserDid:            syntax.DID(did),
		RepoStarred:        r.FormValue("repo_starred") == "on",
		IssueCreated:       r.FormValue("issue_created") == "on",
		IssueCommented:     r.FormValue("issue_commented") == "on",
		IssueClosed:        r.FormValue("issue_closed") == "on",
		PullCreated:        r.FormValue("pull_created") == "on",
		PullCommented:      r.FormValue("pull_commented") == "on",
		PullMerged:         r.FormValue("pull_merged") == "on",
		Followed:           r.FormValue("followed") == "on",
		UserMentioned:      r.FormValue("user_mentioned") == "on",
		EmailNotifications: r.FormValue("email_notifications") == "on",
	}

	err := s.Db.UpdateNotificationPreferences(r.Context(), prefs)
	if err != nil {
		s.Logger.Error("failed to update notification preferences", "err", err)
		s.Pages.Notice(w, "settings-notifications-error", "Unable to save notification preferences.")
		return
	}

	s.Pages.Notice(w, "settings-notifications-success", "Notification preferences saved successfully.")
}

func (s *Settings) keysSettings(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	pubKeys, err := db.GetPublicKeysForDid(s.Db, user.Active.Did)
	if err != nil {
		s.Logger.Error("keys settings", "err", err)
	}

	s.Pages.UserKeysSettings(w, pages.UserKeysSettingsParams{
		LoggedInUser: user,
		PubKeys:      pubKeys,
	})
}

func (s *Settings) emailsSettings(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	emails, err := db.GetAllEmails(s.Db, user.Active.Did)
	if err != nil {
		s.Logger.Error("emails settings", "err", err)
	}

	s.Pages.UserEmailsSettings(w, pages.UserEmailsSettingsParams{
		LoggedInUser: user,
		Emails:       emails,
	})
}

// buildVerificationEmail creates an email.Email struct for verification emails
func (s *Settings) buildVerificationEmail(emailAddr, did, code string) email.Email {
	verifyURL := s.verifyUrl(did, emailAddr, code)

	return email.Email{
		APIKey:  s.Config.Resend.ApiKey,
		From:    s.Config.Resend.SentFrom,
		To:      emailAddr,
		Subject: "Verify your Tangled email",
		Text: `Click the link below (or copy and paste it into your browser) to verify your email address.
` + verifyURL,
		Html: `<p>Click the link (or copy and paste it into your browser) to verify your email address.</p>
<p><a href="` + verifyURL + `">` + verifyURL + `</a></p>`,
	}
}

// sendVerificationEmail handles the common logic for sending verification emails
func (s *Settings) sendVerificationEmail(w http.ResponseWriter, did, emailAddr, code string, errorContext string) error {
	emailToSend := s.buildVerificationEmail(emailAddr, did, code)

	err := email.SendEmail(emailToSend)
	if err != nil {
		s.Logger.Error("sending email", "err", err)
		s.Pages.Notice(w, "settings-emails-error", fmt.Sprintf("Unable to send verification email at this moment, try again later. %s", errorContext))
		return err
	}

	return nil
}

func (s *Settings) emails(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		s.Pages.Notice(w, "settings-emails", "Unimplemented.")
		s.Logger.Warn("emails: unimplemented method")
		return
	case http.MethodPut:
		did := s.OAuth.GetDid(r)
		emAddr := r.FormValue("email")
		emAddr = strings.TrimSpace(emAddr)

		if !email.IsValidEmail(emAddr) {
			s.Pages.Notice(w, "settings-emails-error", "Invalid email address.")
			return
		}

		// check if email already exists in database
		existingEmail, err := db.GetEmail(s.Db, did, emAddr)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			s.Logger.Error("checking for existing email", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to add email at this moment, try again later.")
			return
		}

		if err == nil {
			if existingEmail.Verified {
				s.Pages.Notice(w, "settings-emails-error", "This email is already verified.")
				return
			}

			s.Pages.Notice(w, "settings-emails-error", "This email is already added but not verified. Check your inbox for the verification link.")
			return
		}

		code := uuid.New().String()

		// Begin transaction
		tx, err := s.Db.Begin()
		if err != nil {
			s.Logger.Error("failed to start transaction", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to add email at this moment, try again later.")
			return
		}
		defer tx.Rollback()

		if err := db.AddEmail(tx, models.Email{
			Did:              did,
			Address:          emAddr,
			Verified:         false,
			VerificationCode: code,
		}); err != nil {
			s.Logger.Error("adding email", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to add email at this moment, try again later.")
			return
		}

		if err := s.sendVerificationEmail(w, did, emAddr, code, ""); err != nil {
			return
		}

		// Commit transaction
		if err := tx.Commit(); err != nil {
			s.Logger.Error("failed to commit add-email transaction", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to add email at this moment, try again later.")
			return
		}

		s.Pages.Notice(w, "settings-emails-success", "Click the link in the email we sent you to verify your email address.")
		return
	case http.MethodDelete:
		did := s.OAuth.GetDid(r)
		emailAddr := r.FormValue("email")
		emailAddr = strings.TrimSpace(emailAddr)

		// Begin transaction
		tx, err := s.Db.Begin()
		if err != nil {
			s.Logger.Error("failed to start transaction", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to delete email at this moment, try again later.")
			return
		}
		defer tx.Rollback()

		if err := db.DeleteEmail(tx, did, emailAddr); err != nil {
			s.Logger.Error("deleting email", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to delete email at this moment, try again later.")
			return
		}

		// Commit transaction
		if err := tx.Commit(); err != nil {
			s.Logger.Error("failed to commit delete-email transaction", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to delete email at this moment, try again later.")
			return
		}

		s.Pages.HxLocation(w, "/settings/emails")
		return
	}
}

func (s *Settings) verifyUrl(did string, email string, code string) string {
	return fmt.Sprintf(
		"%s/settings/emails/verify?did=%s&email=%s&code=%s",
		s.Config.Core.BaseUrl(),
		url.QueryEscape(did),
		url.QueryEscape(email),
		url.QueryEscape(code),
	)
}

func (s *Settings) emailsVerify(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()

	// Get the parameters directly from the query
	emailAddr := q.Get("email")
	did := q.Get("did")
	code := q.Get("code")

	valid, err := db.CheckValidVerificationCode(s.Db, did, emailAddr, code)
	if err != nil {
		s.Logger.Error("checking email verification", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Error verifying email. Please try again later.")
		return
	}

	if !valid {
		s.Pages.Notice(w, "settings-emails-error", "Invalid verification code. Please request a new verification email.")
		return
	}

	// Mark email as verified in the database
	if err := db.MarkEmailVerified(s.Db, did, emailAddr); err != nil {
		s.Logger.Error("marking email as verified", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Error updating email verification status. Please try again later.")
		return
	}

	http.Redirect(w, r, "/settings/emails", http.StatusSeeOther)
}

func (s *Settings) emailsVerifyResend(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		s.Pages.Notice(w, "settings-emails-error", "Invalid request method.")
		return
	}

	did := s.OAuth.GetDid(r)
	emAddr := r.FormValue("email")
	emAddr = strings.TrimSpace(emAddr)

	if !email.IsValidEmail(emAddr) {
		s.Pages.Notice(w, "settings-emails-error", "Invalid email address.")
		return
	}

	// Check if email exists and is unverified
	existingEmail, err := db.GetEmail(s.Db, did, emAddr)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			s.Pages.Notice(w, "settings-emails-error", "Email not found. Please add it first.")
		} else {
			s.Logger.Error("checking for existing email", "err", err)
			s.Pages.Notice(w, "settings-emails-error", "Unable to resend verification email at this moment, try again later.")
		}
		return
	}

	if existingEmail.Verified {
		s.Pages.Notice(w, "settings-emails-error", "This email is already verified.")
		return
	}

	// Check if last verification email was sent less than 10 minutes ago
	if existingEmail.LastSent != nil {
		timeSinceLastSent := time.Since(*existingEmail.LastSent)
		if timeSinceLastSent < 10*time.Minute {
			waitTime := 10*time.Minute - timeSinceLastSent
			s.Pages.Notice(w, "settings-emails-error", fmt.Sprintf("Please wait %d minutes before requesting another verification email.", int(waitTime.Minutes()+1)))
			return
		}
	}

	// Generate new verification code
	code := uuid.New().String()

	// Begin transaction
	tx, err := s.Db.Begin()
	if err != nil {
		s.Logger.Error("failed to start transaction", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Unable to resend verification email at this moment, try again later.")
		return
	}
	defer tx.Rollback()

	// Update the verification code and last sent time
	if err := db.UpdateVerificationCode(tx, did, emAddr, code); err != nil {
		s.Logger.Error("updating email verification code", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Unable to resend verification email at this moment, try again later.")
		return
	}

	// Send verification email
	if err := s.sendVerificationEmail(w, did, emAddr, code, ""); err != nil {
		return
	}

	// Commit transaction
	if err := tx.Commit(); err != nil {
		s.Logger.Error("failed to commit resend-verification transaction", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Unable to resend verification email at this moment, try again later.")
		return
	}

	s.Pages.Notice(w, "settings-emails-success", "Verification email resent. Click the link in the email we sent you to verify your email address.")
}

func (s *Settings) emailsPrimary(w http.ResponseWriter, r *http.Request) {
	did := s.OAuth.GetDid(r)
	emailAddr := r.FormValue("email")
	emailAddr = strings.TrimSpace(emailAddr)

	if emailAddr == "" {
		s.Pages.Notice(w, "settings-emails-error", "Email address cannot be empty.")
		return
	}

	if err := db.MakeEmailPrimary(s.Db, did, emailAddr); err != nil {
		s.Logger.Error("setting primary email", "err", err)
		s.Pages.Notice(w, "settings-emails-error", "Error setting primary email. Please try again later.")
		return
	}

	s.Pages.HxLocation(w, "/settings/emails")
}

func (s *Settings) keys(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		s.Pages.Notice(w, "settings-keys", "Unimplemented.")
		s.Logger.Warn("keys: unimplemented method")
		return
	case http.MethodPut:
		did := s.OAuth.GetDid(r)
		key := r.FormValue("key")
		key = strings.TrimSpace(key)
		name := r.FormValue("name")
		client, err := s.OAuth.AuthorizedClient(r)
		if err != nil {
			s.Pages.Notice(w, "settings-keys", "Failed to authorize. Try again later.")
			return
		}

		_, _, _, _, err = ssh.ParseAuthorizedKey([]byte(key))
		if err != nil {
			s.Logger.Error("parsing public key", "err", err)
			s.Pages.Notice(w, "settings-keys", "That doesn't look like a valid public key. Make sure it's a <strong>public</strong> key.")
			return
		}

		rkey := tid.TID()

		tx, err := s.Db.Begin()
		if err != nil {
			s.Logger.Error("failed to start transaction for adding public key", "err", err)
			s.Pages.Notice(w, "settings-keys", "Unable to add public key at this moment, try again later.")
			return
		}
		defer tx.Rollback()

		if err := db.AddPublicKey(tx, did, name, key, rkey); err != nil {
			s.Logger.Error("adding public key", "err", err)
			s.Pages.Notice(w, "settings-keys", "Failed to add public key.")
			return
		}

		// store in pds too
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.PublicKeyNSID,
			Repo:       did,
			Rkey:       rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.PublicKey{
					CreatedAt: time.Now().Format(time.RFC3339),
					Key:       key,
					Name:      name,
				}},
		})
		// invalid record
		if err != nil {
			s.Logger.Error("failed to create atproto record", "err", err)
			s.Pages.Notice(w, "settings-keys", "Failed to create record.")
			return
		}

		s.Logger.Info("created atproto record", "uri", resp.Uri)

		err = tx.Commit()
		if err != nil {
			s.Logger.Error("failed to commit add-key transaction", "err", err)
			s.Pages.Notice(w, "settings-keys", "Unable to add public key at this moment, try again later.")
			return
		}

		s.Pages.HxLocation(w, "/settings/keys")
		return

	case http.MethodDelete:
		did := s.OAuth.GetDid(r)
		q := r.URL.Query()

		name := q.Get("name")
		rkey := q.Get("rkey")
		key := q.Get("key")

		s.Logger.Debug("deleting key", "name", name, "rkey", rkey, "key", key)

		client, err := s.OAuth.AuthorizedClient(r)
		if err != nil {
			s.Logger.Error("failed to authorize client", "err", err)
			s.Pages.Notice(w, "settings-keys", "Failed to authorize client.")
			return
		}

		if err := db.DeletePublicKey(s.Db, did, name, key); err != nil {
			s.Logger.Error("removing public key", "err", err)
			s.Pages.Notice(w, "settings-keys", "Failed to remove public key.")
			return
		}

		if rkey != "" {
			// remove from pds too
			_, err := comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
				Collection: tangled.PublicKeyNSID,
				Repo:       did,
				Rkey:       rkey,
			})

			// invalid record
			if err != nil {
				s.Logger.Error("failed to delete record from PDS", "err", err)
				s.Pages.Notice(w, "settings-keys", "Failed to remove key from PDS.")
				return
			}
		}
		s.Logger.Info("deleted key successfully", "name", name)

		s.Pages.HxLocation(w, "/settings/keys")
		return
	}
}
