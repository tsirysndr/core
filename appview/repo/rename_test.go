package repo

import (
	"strings"
	"testing"
)

func TestValidateRenameInput(t *testing.T) {
	const validTID = "3jzfcijpj2z2a"
	cases := []struct {
		name        string
		currentName string
		currentRkey string
		raw         string
		wantName    string
		wantErrSub  string
	}{
		{"happy path", "foo", "", "bar", "bar", ""},
		{"trims surrounding whitespace", "foo", "", "  bar  ", "bar", ""},
		{"strips .git suffix", "foo", "", "bar.git", "bar", ""},
		{"empty after trim", "foo", "", "   ", "", "cannot be empty"},
		{"raw empty", "foo", "", "", "", "cannot be empty"},
		{"path traversal slash", "foo", "", "../bar", "", "invalid path"},
		{"invalid character", "foo", "", "ba r", "", "alphanumeric"},
		{"same name as current with non-TID rkey", "foo", "foo", "foo", "", "matches the current name"},
		{"same name as current with TID rkey allowed", "foo", validTID, "foo", "foo", ""},
		{"case-only diff is not a no-op", "foo", "", "Foo", "Foo", ""},
		{"strip-git collides with current", "foo", "", "foo.git", "", "matches the current name"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := validateRenameInput(tc.currentName, tc.currentRkey, tc.raw)
			if tc.wantErrSub == "" {
				if err != nil {
					t.Fatalf("err = %v, want nil", err)
				}
				if got != tc.wantName {
					t.Errorf("name = %q, want %q", got, tc.wantName)
				}
				return
			}
			if err == nil {
				t.Fatalf("err = nil, want error containing %q", tc.wantErrSub)
			}
			if got != "" {
				t.Errorf("name = %q, want empty on error", got)
			}
			if !strings.Contains(err.Error(), tc.wantErrSub) {
				t.Errorf("err = %q, want substring %q", err.Error(), tc.wantErrSub)
			}
		})
	}
}
