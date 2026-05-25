package sandbox

import (
	"os/exec"
	"testing"
)

func TestNoopBackend_Wrap(t *testing.T) {
	sb := &NoopBackend{}
	cmd := exec.Command("git", "status")

	wrapped, err := sb.Wrap("/some/repo", cmd)
	if err != nil {
		t.Fatalf("Wrap: %v", err)
	}
	if wrapped != cmd {
		t.Error("Wrap should return the same cmd, not a new one")
	}
	if wrapped.Dir != "/some/repo" {
		t.Errorf("Dir = %q, want %q", wrapped.Dir, "/some/repo")
	}
}

func TestNoopBackend_WrapMulti(t *testing.T) {
	sb := &NoopBackend{}
	cmd := exec.Command("git", "merge")

	wrapped, err := sb.WrapMulti([]string{"/a", "/b"}, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.Dir != "/a" {
		t.Errorf("Dir = %q, want %q (first path)", wrapped.Dir, "/a")
	}
}

func TestNoopBackend_WrapMulti_Empty(t *testing.T) {
	sb := &NoopBackend{}
	cmd := exec.Command("git", "status")
	cmd.Dir = "/preserved"

	wrapped, err := sb.WrapMulti(nil, cmd)
	if err != nil {
		t.Fatalf("WrapMulti: %v", err)
	}
	if wrapped.Dir != "/preserved" {
		t.Errorf("empty paths should not overwrite cmd.Dir; got %q", wrapped.Dir)
	}
}

func TestNoopBackend_Name(t *testing.T) {
	if (&NoopBackend{}).Name() != "noop" {
		t.Error("Name should return \"noop\"")
	}
}
