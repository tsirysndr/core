//go:build linux

package microvm

import "testing"

func TestDebugSSHCommandUsesJumpHost(t *testing.T) {
	got := debugSSHCommand("0.0.0.0:2224", "executor-a.internal", "127.0.0.1", "spindle.example", "job-1")
	want := "ssh -tt -J spindle.example -p 2224 job-1@127.0.0.1"
	if got != want {
		t.Fatalf("debug ssh command = %q, want %q", got, want)
	}
}

func TestDebugSSHCommandUsesExecutorHostWithoutJump(t *testing.T) {
	got := debugSSHCommand("0.0.0.0:22", "executor-a.example", "", "", "job-1")
	want := "ssh -tt job-1@executor-a.example"
	if got != want {
		t.Fatalf("debug ssh command = %q, want %q", got, want)
	}
}
