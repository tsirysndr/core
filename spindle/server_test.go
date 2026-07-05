package spindle

import (
	"testing"

	kgit "tangled.org/core/knotserver/git"
)

func TestHasSkipCIPushOption(t *testing.T) {
	tests := []struct {
		name        string
		pushOptions []string
		want        bool
	}{
		{
			name:        "skip-ci requests skip",
			pushOptions: []string{"skip-ci"},
			want:        true,
		},
		{
			name:        "ci-skip requests skip",
			pushOptions: []string{"ci-skip"},
			want:        true,
		},
		{
			name:        "unrelated ci options do not skip",
			pushOptions: []string{"verbose-ci", "ci-verbose"},
			want:        false,
		},
		{
			name:        "empty options do not skip",
			pushOptions: []string{},
			want:        false,
		},
		{
			name:        "nil options do not skip",
			pushOptions: nil,
			want:        false,
		},
		{
			name:        "mixed options skip when any skip option appears",
			pushOptions: []string{"verbose-ci", "skip-ci", "ci-verbose"},
			want:        true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := kgit.HasSkipCIPushOption(tt.pushOptions)
			if got != tt.want {
				t.Fatalf("hasSkipCIPushOption(%v) = %v, want %v", tt.pushOptions, got, tt.want)
			}
		})
	}
}
