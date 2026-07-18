package placement

import (
	"testing"
)

func TestIsNativeArchitecture(t *testing.T) {
	tests := []struct {
		image string
		host  string
		want  bool
	}{
		{image: "x86_64", host: "amd64", want: true},
		{image: "amd64", host: "amd64", want: true},
		{image: "aarch64", host: "arm64", want: true},
		{image: "arm64", host: "arm64", want: true},
		{image: "x86_64", host: "arm64", want: false},
		{image: "aarch64", host: "amd64", want: false},
		{image: "", host: "amd64", want: false},
	}
	for _, tt := range tests {
		if got := IsNativeArchitecture(tt.image, tt.host); got != tt.want {
			t.Errorf("IsNativeArchitecture(%q, %q) = %v, want %v", tt.image, tt.host, got, tt.want)
		}
	}
}
