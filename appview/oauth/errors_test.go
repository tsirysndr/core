package oauth

import (
	"errors"
	"fmt"
	"testing"
)

func TestIsPermanentAuthErr(t *testing.T) {
	cases := []struct {
		name string
		err  error
		want bool
	}{
		{"nil", nil, false},
		{"empty", errors.New(""), false},
		{"random", errors.New("network unreachable"), false},
		{"rate limited", errors.New("token refresh failed (HTTP 429): rate_limited"), false},
		{"invalid grant direct", errors.New("token refresh failed (HTTP 400): invalid_grant"), true},
		{"invalid grant wrapped", fmt.Errorf("put record: %w", errors.New("failed to refresh OAuth tokens: token refresh failed: auth server request failed (HTTP 400): invalid_grant")), true},
		{"invalid client", errors.New("auth server request failed (HTTP 401): invalid_client"), true},
		{"unauthorized client", errors.New("token refresh failed (HTTP 400): unauthorized_client"), true},
		{"substring trap", errors.New("our invalid_grant_alternative ran out"), false},
		{"case-sensitive", errors.New("INVALID_GRANT"), false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := IsPermanentAuthErr(c.err)
			if got != c.want {
				t.Fatalf("got %v want %v", got, c.want)
			}
		})
	}
}

func TestIsStaleAccessTokenErr(t *testing.T) {
	cases := []struct {
		name string
		err  error
		want bool
	}{
		{"nil", nil, false},
		{"random", errors.New("hello"), false},
		{"500", errors.New("API request failed (HTTP 500): InternalError"), false},
		{"401 auth required", errors.New("API request failed (HTTP 401): AuthenticationRequired: Invalid OAuth access token"), true},
		{"401 invalid token", errors.New("API request failed (HTTP 401): invalid_token"), true},
		{"401 wrapped", fmt.Errorf("put record: %w", errors.New("API request failed (HTTP 401): AuthenticationRequired")), true},
		{"403 forbidden", errors.New("API request failed (HTTP 403): Forbidden"), false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := IsStaleAccessTokenErr(c.err)
			if got != c.want {
				t.Fatalf("got %v want %v", got, c.want)
			}
		})
	}
}
