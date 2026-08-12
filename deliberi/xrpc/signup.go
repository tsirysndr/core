package xrpc

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"

	"tangled.org/core/api/tangled"
	appviewemail "tangled.org/core/appview/email"
	db "tangled.org/core/deliberi/db"
	"tangled.org/core/deliberi/models"
)

// subdomainRegex validates the requested pds handle label
var subdomainRegex = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{2,61}[a-z0-9])?$`)

func isValidSubdomain(name string) bool {
	return len(name) >= 4 && len(name) <= 63 && subdomainRegex.MatchString(name)
}

func (x *Xrpc) AccountBeginSignup(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AccountBeginSignup")

	// signup needs a pds admin secret (turnstile is enforced upstream)
	if !x.Config.SignupEnabled() {
		writeError(w, xrpcErrorTag("SignupDisabled", "signup is not currently enabled"), http.StatusFailedDependency)
		return
	}

	var input tangled.TempAccountBeginSignup_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	if !appviewemail.IsValidEmail(input.Email) {
		writeError(w, xrpcErrorTag("InvalidEmail", "invalid email address"), http.StatusBadRequest)
		return
	}

	exists, err := db.CheckEmailExistsAtAll(x.DB, input.Email)
	if err != nil {
		l.Error("failed to check email existence", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}
	if exists {
		writeError(w, xrpcErrorTag("EmailAlreadyRegistered", "an account already exists for this email"), http.StatusConflict)
		return
	}

	// the verification code is an invite code minted by the PDS
	code, err := x.pdsCreateInviteCode()
	if err != nil {
		l.Error("failed to create invite code", "err", err)
		writeError(w, errUpstream, http.StatusBadGateway)
		return
	}

	if err := db.AddInflightSignup(x.DB, models.InflightSignup{Email: input.Email, InviteCode: code}); err != nil {
		l.Error("failed to add inflight signup", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	// deliberi owns email now: send the verification code inline (stdout in dev
	// when no resend key is set).
	text := "Copy and paste this code below to verify your account on Tangled.\n" + code
	html := "<p>Copy and paste this code below to verify your account on Tangled.</p>\n<p><code>" + code + "</code></p>"
	if err := x.Sender.Send(input.Email, "Verify your Tangled account", text, html); err != nil {
		l.Error("failed to send verification email", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) AccountCompleteSignup(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "AccountCompleteSignup")

	if !x.Config.SignupEnabled() {
		writeError(w, xrpcErrorTag("SignupDisabled", "signup is not currently enabled"), http.StatusFailedDependency)
		return
	}

	var input tangled.TempAccountCompleteSignup_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	if !isValidSubdomain(input.Username) {
		writeError(w, xrpcErrorTag("InvalidUsername", "invalid username"), http.StatusBadRequest)
		return
	}

	emailAddr, err := db.GetEmailForCode(x.DB, input.Code)
	if err != nil {
		l.Error("failed to get email for code", "err", err)
		writeError(w, xrpcErrorTag("InvalidCode", "invalid or expired verification code"), http.StatusBadRequest)
		return
	}

	did, handle, err := x.provisionAccount(input.Username, input.Password, emailAddr, input.Code)
	if err != nil {
		l.Error("failed to provision account", "err", err)
		writeError(w, errUpstream, http.StatusBadGateway)
		return
	}

	go func() {
		if err := db.DeleteInflightSignup(x.DB, emailAddr); err != nil {
			l.Error("failed to delete inflight signup", "err", err)
		}
	}()

	x.writeJSON(w, &tangled.TempAccountCompleteSignup_Output{Did: did, Handle: handle})
}

// provisionAccount creates the pds account and records its verified primary
// email, rolling back on failure.
func (x *Xrpc) provisionAccount(username, password, emailAddr, code string) (did, handle string, err error) {
	success := false
	emailAdded := false
	defer func() {
		if success {
			return
		}
		x.Logger.Info("rolling back signup", "username", username, "did", did)
		if did != "" {
			if derr := x.pdsDeleteAccount(did); derr != nil {
				x.Logger.Error("failed to roll back PDS account", "err", derr, "did", did)
			}
		}
		if emailAdded {
			if derr := db.DeleteEmail(x.DB, did, emailAddr); derr != nil {
				x.Logger.Error("failed to roll back email row", "err", derr, "email", emailAddr)
			}
		}
	}()

	did, handle, err = x.pdsCreateAccount(username, password, emailAddr, code)
	if err != nil {
		return "", "", err
	}

	if err = db.AddEmail(x.DB, models.Email{Did: did, Address: emailAddr, Verified: true, Primary: true}); err != nil {
		return "", "", err
	}
	emailAdded = true

	// sites subdomain auto-claim now belongs elsewhere; deliberi does not own the sites table

	success = true
	return did, handle, nil
}

// pdsRequest posts to a pds xrpc endpoint; useAuth sends the admin secret via
// basic auth. these are unauth'd or admin-authed, so they use raw http.
func (x *Xrpc) pdsRequest(endpoint string, body any, useAuth bool) (*http.Response, error) {
	jsonData, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	u := fmt.Sprintf("%s/xrpc/%s", x.Config.Pds.Host, endpoint)
	req, err := http.NewRequest(http.MethodPost, u, bytes.NewBuffer(jsonData))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if useAuth {
		req.SetBasicAuth("admin", x.Config.Pds.AdminSecret)
	}
	return http.DefaultClient.Do(req)
}

func pdsError(resp *http.Response, action string) error {
	var e struct {
		Error   string `json:"error"`
		Message string `json:"message"`
	}
	b, _ := io.ReadAll(resp.Body)
	if err := json.Unmarshal(b, &e); err == nil && e.Message != "" {
		return fmt.Errorf("failed to %s: %s - %s", action, e.Error, e.Message)
	}
	return fmt.Errorf("failed to %s, status %d", action, resp.StatusCode)
}

func (x *Xrpc) pdsCreateInviteCode() (string, error) {
	resp, err := x.pdsRequest("com.atproto.server.createInviteCode", map[string]any{"useCount": 1}, true)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", pdsError(resp, "create invite code")
	}
	var result map[string]string
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return "", fmt.Errorf("failed to decode invite code response: %w", err)
	}
	return result["code"], nil
}

func (x *Xrpc) pdsCreateAccount(username, password, emailAddr, code string) (did, handle string, err error) {
	parsed, err := url.Parse(x.Config.Pds.Host)
	if err != nil {
		return "", "", fmt.Errorf("invalid PDS host URL: %w", err)
	}
	handle = fmt.Sprintf("%s.%s", username, parsed.Hostname())

	body := map[string]string{
		"email":      emailAddr,
		"handle":     handle,
		"password":   password,
		"inviteCode": code,
	}
	resp, err := x.pdsRequest("com.atproto.server.createAccount", body, false)
	if err != nil {
		return "", "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", "", pdsError(resp, "create account")
	}

	var result struct {
		DID string `json:"did"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return "", "", fmt.Errorf("failed to decode create account response: %w", err)
	}
	return result.DID, handle, nil
}

func (x *Xrpc) pdsDeleteAccount(did string) error {
	resp, err := x.pdsRequest("com.atproto.admin.deleteAccount", map[string]string{"did": did}, true)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return pdsError(resp, "delete account")
	}
	return nil
}
