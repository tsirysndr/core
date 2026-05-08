package compat113

import "testing"

func TestAtLeast114(t *testing.T) {
	cases := []struct {
		in   string
		want bool
	}{
		{"v1.14.0", true},
		{"v1.14.0-alpha", true},
		{"v1.14.5", true},
		{"v1.13.0", false},
		{"v1.13.0-alpha", false},
		{"v1.0.0", false},
		{"v2.0.0", true},
		{"1.14.0", true},
		{"1.13.99", false},
		{"(devel)", true},
		{"", false},
		{"garbagio-furioso", false},
		{"v1", false},
		{"vX.Y.Z", false},
		{"unknown", false},
		{"unknown-abc1234", false},
		{"unknown-abc1234-modified", false},
	}
	for _, c := range cases {
		t.Run(c.in, func(t *testing.T) {
			if got := atLeast114(c.in); got != c.want {
				t.Errorf("atLeast114(%q) = %v, want %v", c.in, got, c.want)
			}
		})
	}
}
