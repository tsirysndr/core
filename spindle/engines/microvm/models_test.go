//go:build linux

package microvm

import (
	"slices"
	"testing"
)

func TestWorkflowCaches(t *testing.T) {
	urls, keys, err := workflowCaches(map[string]string{
		"https://hydra.nixos.org/":  "hydra.nixos.org-1:CNHJZBh9K4tP3EKF6FkkgeVYsS3ohTl+oS0Qa8bezVs=",
		"https://cache.garnix.io/":  "cache.garnix.io:CTFPyKSLcx5RMJKfLo5EEPUObbA78b0YQ2DTCJXqr9g=",
		"https://unsigned.example/": "",
	})
	if err != nil {
		t.Fatal(err)
	}

	wantURLs := []string{
		"https://cache.garnix.io/",
		"https://hydra.nixos.org/",
		"https://unsigned.example/",
	}
	if !slices.Equal(urls, wantURLs) {
		t.Fatalf("urls: got %v, want %v", urls, wantURLs)
	}
	wantKeys := []string{
		"cache.garnix.io:CTFPyKSLcx5RMJKfLo5EEPUObbA78b0YQ2DTCJXqr9g=",
		"hydra.nixos.org-1:CNHJZBh9K4tP3EKF6FkkgeVYsS3ohTl+oS0Qa8bezVs=",
	}
	if !slices.Equal(keys, wantKeys) {
		t.Fatalf("keys: got %v, want %v", keys, wantKeys)
	}
}

func TestWorkflowCachesRejectsBadURLs(t *testing.T) {
	for _, bad := range []string{"ftp://cache.example/", "not a url"} {
		if _, _, err := workflowCaches(map[string]string{bad: ""}); err == nil {
			t.Errorf("workflowCaches(%q): expected error, got nil", bad)
		}
	}
}
