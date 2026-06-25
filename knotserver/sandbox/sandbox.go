package sandbox

import (
	"fmt"
	"os"
	"os/exec"
)

// Backend wraps git subprocesses in a filesystem sandbox.
type Backend interface {
	Wrap(repoPath string, cmd *exec.Cmd) (*exec.Cmd, error)
	WrapMulti(paths []string, cmd *exec.Cmd) (*exec.Cmd, error)
	Name() string
}

// NoopBackend passes commands through unchanged.
type NoopBackend struct{}

func (n *NoopBackend) Wrap(repoPath string, cmd *exec.Cmd) (*exec.Cmd, error) {
	cmd.Env = append(cmd.Env, fmt.Sprintf("HOME=%s", os.Getenv("HOME")))
	cmd.Dir = repoPath
	return cmd, nil
}

func (n *NoopBackend) WrapMulti(paths []string, cmd *exec.Cmd) (*exec.Cmd, error) {
	if len(paths) > 0 {
		cmd.Dir = paths[0]
	}
	return cmd, nil
}

func (n *NoopBackend) Name() string { return "noop" }

// LookupUID resolves a repo path to its owner virtual UID. Used by the sandbox
// to drop privileges before running git. Returning 0 (or any error) means
// don't drop, i.e. the subprocess runs as the calling user.
type LookupUID func(repoPath string) (uid uint32, gid uint32, err error)

// New returns the best available sandboxing backend. If landlock is not
// available, the warning string is non-empty and the backend falls back
// to NoopBackend. lookup is optional; nil means subprocesses keep the
// caller's UID/GID.
func New(lookup LookupUID) (Backend, string) {
	return platformNew(lookup)
}

// Probe returns a human-readable description of sandbox capability on this host.
func Probe() string {
	return platformProbe()
}
