package knotserver

import (
	"testing"

	"tangled.org/core/knotserver/git"
)

func TestHasVerboseCIPushOption(t *testing.T) {
	cases := []struct {
		name        string
		pushOptions []string
		want        bool
	}{
		{"verbose-ci token", []string{"verbose-ci"}, true},
		{"ci-verbose token", []string{"ci-verbose"}, true},
		{"verbose token among others", []string{"foo", "ci-verbose", "bar"}, true},
		{"skip-ci is not verbose", []string{"skip-ci"}, false},
		{"ci-skip is not verbose", []string{"ci-skip"}, false},
		{"unrelated token", []string{"whatever"}, false},
		{"empty slice", []string{}, false},
		{"nil slice", nil, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := hasVerboseCIPushOption(tc.pushOptions); got != tc.want {
				t.Errorf("hasVerboseCIPushOption(%v) = %v, want %v", tc.pushOptions, got, tc.want)
			}
		})
	}
}

func TestHasSkipCIPushOption(t *testing.T) {
	cases := []struct {
		name        string
		pushOptions []string
		want        bool
	}{
		{"skip-ci token", []string{"skip-ci"}, true},
		{"ci-skip token", []string{"ci-skip"}, true},
		{"skip token among others", []string{"foo", "skip-ci", "bar"}, true},
		{"verbose-ci is not skip", []string{"verbose-ci"}, false},
		{"ci-verbose is not skip", []string{"ci-verbose"}, false},
		{"unrelated token", []string{"whatever"}, false},
		{"empty slice", []string{}, false},
		{"nil slice", nil, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := git.HasSkipCIPushOption(tc.pushOptions); got != tc.want {
				t.Errorf("hasSkipCIPushOption(%v) = %v, want %v", tc.pushOptions, got, tc.want)
			}
		})
	}
}
