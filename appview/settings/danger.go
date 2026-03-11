package settings

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/xrpc"
)

type pdsSession struct {
	Client    *xrpc.Client
	Did       string
	Email     string
	AccessJwt string
}

func (s *Settings) pdsClient() *xrpc.Client {
	return &xrpc.Client{
		Host:   s.Config.Pds.Host,
		Client: &http.Client{Timeout: 15 * time.Second},
	}
}

func (s *Settings) verifyPdsPassword(did, password string) (*pdsSession, error) {
	client := s.pdsClient()
	resp, err := comatproto.ServerCreateSession(context.Background(), client, &comatproto.ServerCreateSession_Input{
		Identifier: did,
		Password:   password,
	})
	if err != nil {
		return nil, err
	}

	client.Auth = &xrpc.AuthInfo{AccessJwt: resp.AccessJwt}

	var email string
	if resp.Email != nil {
		email = *resp.Email
	}

	return &pdsSession{
		Client:    client,
		Did:       resp.Did,
		Email:     email,
		AccessJwt: resp.AccessJwt,
	}, nil
}

func (s *Settings) revokePdsSession(session *pdsSession) {
	if err := comatproto.ServerDeleteSession(context.Background(), session.Client); err != nil {
		s.Logger.Warn("failed to revoke session", "err", err)
	}
}

func (s *Settings) requestPasswordReset(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "password-error", "Only available for tngl.sh accounts.")
		return
	}

	did := s.OAuth.GetDid(r)
	password := r.FormValue("current_password")
	if password == "" {
		s.Pages.Notice(w, "password-error", "Password is required.")
		return
	}

	session, err := s.verifyPdsPassword(did, password)
	if err != nil {
		s.Pages.Notice(w, "password-error", "Current password is incorrect.")
		return
	}

	if session.Email == "" {
		s.revokePdsSession(session)
		s.Logger.Error("requesting password reset: no email on account", "did", did)
		s.Pages.Notice(w, "password-error", "No email associated with your account.")
		return
	}

	s.revokePdsSession(session)

	err = comatproto.ServerRequestPasswordReset(context.Background(), s.pdsClient(), &comatproto.ServerRequestPasswordReset_Input{
		Email: session.Email,
	})
	if err != nil {
		s.Logger.Error("requesting password reset", "err", err)
		s.Pages.Notice(w, "password-error", "Failed to request password reset. Try again later.")
		return
	}

	s.Pages.DangerPasswordTokenStep(w)
}

func (s *Settings) resetPassword(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "password-error", "Only available for tngl.sh accounts.")
		return
	}

	token := strings.TrimSpace(r.FormValue("token"))
	newPassword := r.FormValue("new_password")
	confirmPassword := r.FormValue("confirm_password")

	if token == "" || newPassword == "" || confirmPassword == "" {
		s.Pages.Notice(w, "password-error", "All fields are required.")
		return
	}

	if newPassword != confirmPassword {
		s.Pages.Notice(w, "password-error", "Passwords do not match.")
		return
	}

	err := comatproto.ServerResetPassword(context.Background(), s.pdsClient(), &comatproto.ServerResetPassword_Input{
		Token:    token,
		Password: newPassword,
	})
	if err != nil {
		s.Logger.Error("resetting password", "err", err)
		s.Pages.Notice(w, "password-error", "Failed to reset password. The token may have expired.")
		return
	}

	s.Pages.DangerPasswordSuccess(w)
}

func (s *Settings) deactivateAccount(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "deactivate-error", "Only available for tngl.sh accounts.")
		return
	}

	did := s.OAuth.GetDid(r)
	password := r.FormValue("password")

	if password == "" {
		s.Pages.Notice(w, "deactivate-error", "Password is required.")
		return
	}

	session, err := s.verifyPdsPassword(did, password)
	if err != nil {
		s.Pages.Notice(w, "deactivate-error", "Password is incorrect.")
		return
	}

	err = comatproto.ServerDeactivateAccount(context.Background(), session.Client, &comatproto.ServerDeactivateAccount_Input{})
	s.revokePdsSession(session)
	if err != nil {
		s.Logger.Error("deactivating account", "err", err)
		s.Pages.Notice(w, "deactivate-error", "Failed to deactivate account. Try again later.")
		return
	}

	if err := s.OAuth.DeleteSession(w, r); err != nil {
		s.Logger.Error("clearing session after deactivation", "did", did, "err", err)
	}
	if err := s.OAuth.RemoveAccount(w, r, did); err != nil {
		s.Logger.Error("removing account after deactivation", "did", did, "err", err)
	}
	s.Pages.HxRedirect(w, "/")
}

func (s *Settings) requestAccountDelete(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "delete-error", "Only available for tngl.sh accounts.")
		return
	}

	did := s.OAuth.GetDid(r)
	password := r.FormValue("password")

	if password == "" {
		s.Pages.Notice(w, "delete-error", "Password is required.")
		return
	}

	session, err := s.verifyPdsPassword(did, password)
	if err != nil {
		s.Pages.Notice(w, "delete-error", "Password is incorrect.")
		return
	}

	err = comatproto.ServerRequestAccountDelete(context.Background(), session.Client)
	s.revokePdsSession(session)
	if err != nil {
		s.Logger.Error("requesting account deletion", "err", err)
		s.Pages.Notice(w, "delete-error", "Failed to request account deletion. Try again later.")
		return
	}

	s.Pages.DangerDeleteTokenStep(w)
}

func (s *Settings) deleteAccount(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "delete-error", "Only available for tngl.sh accounts.")
		return
	}

	did := s.OAuth.GetDid(r)
	password := r.FormValue("password")
	token := strings.TrimSpace(r.FormValue("token"))
	confirmation := r.FormValue("confirmation")

	if password == "" || token == "" {
		s.Pages.Notice(w, "delete-error", "All fields are required.")
		return
	}

	if confirmation != "delete my account" {
		s.Pages.Notice(w, "delete-error", "You must type \"delete my account\" to confirm.")
		return
	}

	err := comatproto.ServerDeleteAccount(context.Background(), s.pdsClient(), &comatproto.ServerDeleteAccount_Input{
		Did:      did,
		Password: password,
		Token:    token,
	})
	if err != nil {
		s.Logger.Error("deleting account", "err", err)
		s.Pages.Notice(w, "delete-error", "Failed to delete account. Try again later.")
		return
	}

	if err := s.OAuth.DeleteSession(w, r); err != nil {
		s.Logger.Error("clearing session after account deletion", "did", did, "err", err)
	}
	if err := s.OAuth.RemoveAccount(w, r, did); err != nil {
		s.Logger.Error("removing account after deletion", "did", did, "err", err)
	}
	s.Pages.HxRedirect(w, "/")
}

func (s *Settings) isAccountDeactivated(ctx context.Context, did, pdsHost string) bool {
	client := &xrpc.Client{
		Host:   pdsHost,
		Client: &http.Client{Timeout: 5 * time.Second},
	}

	_, err := comatproto.RepoDescribeRepo(ctx, client, did)
	if err == nil {
		return false
	}

	var xrpcErr *xrpc.Error
	var xrpcBody *xrpc.XRPCError
	return errors.As(err, &xrpcErr) &&
		errors.As(xrpcErr.Wrapped, &xrpcBody) &&
		xrpcBody.ErrStr == "RepoDeactivated"
}

func (s *Settings) reactivateAccount(w http.ResponseWriter, r *http.Request) {
	user := s.OAuth.GetMultiAccountUser(r)
	if !s.Config.Pds.IsTnglShUser(user.Pds()) {
		s.Pages.Notice(w, "reactivate-error", "Only available for tngl.sh accounts.")
		return
	}

	did := s.OAuth.GetDid(r)
	password := r.FormValue("password")

	if password == "" {
		s.Pages.Notice(w, "reactivate-error", "Password is required.")
		return
	}

	session, err := s.verifyPdsPassword(did, password)
	if err != nil {
		s.Pages.Notice(w, "reactivate-error", "Password is incorrect.")
		return
	}

	err = comatproto.ServerActivateAccount(context.Background(), session.Client)
	s.revokePdsSession(session)
	if err != nil {
		s.Logger.Error("reactivating account", "err", err)
		s.Pages.Notice(w, "reactivate-error", "Failed to reactivate account. Try again later.")
		return
	}

	s.Pages.HxRefresh(w)
}
