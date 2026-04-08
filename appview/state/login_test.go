package state

import "testing"

func TestSanitizeReturnURL(t *testing.T) {
	cases := []struct {
		input string
		want  string
	}{
		{"/", "/"},
		{"/some/path", "/some/path"},
		{"/valid?query=1", "/valid?query=1"},
		{"/valid#anchor", "/valid#anchor"},
		// External URLs must be rejected.
		{"https://evil.com", "/"},
		{"http://evil.com", "/"},
		// Protocol-relative URLs are treated as external by browsers.
		{"//evil.com", "/"},
		{"//evil.com/phishing", "/"},
		// Empty string.
		{"", "/"},
	}
	for _, tc := range cases {
		if got := sanitizeReturnURL(tc.input); got != tc.want {
			t.Errorf("sanitizeReturnURL(%q) = %q, want %q", tc.input, got, tc.want)
		}
	}
}
