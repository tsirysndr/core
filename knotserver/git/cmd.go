package git

import (
	"bytes"
	"fmt"
	"io"
	"os/exec"
	"strings"
	"syscall"
)

const (
	fieldSeparator  = "\x1f" // ASCII Unit Separator
	recordSeparator = "\x1e" // ASCII Record Separator
)

func (g *GitRepo) runGitCmd(command string, extraArgs ...string) ([]byte, error) {
	var args []string
	args = append(args, command)
	args = append(args, extraArgs...)

	cmd := exec.Command("git", args...)

	if g.sandbox != nil {
		var wrapErr error
		cmd, wrapErr = g.sandbox.Wrap(g.path, cmd)
		if wrapErr != nil {
			return nil, fmt.Errorf("sandbox wrap: %w", wrapErr)
		}
	} else {
		cmd.Dir = g.path
	}

	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	}

	out, err := cmd.Output()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			return nil, fmt.Errorf("%w, stderr: %s", err, string(exitErr.Stderr))
		}
		return nil, err
	}

	return out, nil
}

func (g *GitRepo) revList(extraArgs ...string) ([]byte, error) {
	return g.runGitCmd("rev-list", extraArgs...)
}

func (g *GitRepo) forEachRef(extraArgs ...string) ([]byte, error) {
	return g.runGitCmd("for-each-ref", extraArgs...)
}

func (g *GitRepo) revParse(extraArgs ...string) ([]byte, error) {
	return g.runGitCmd("rev-parse", extraArgs...)
}

func (g *GitRepo) mergeBase(extraArgs ...string) ([]byte, error) {
	return g.runGitCmd("merge-base", extraArgs...)
}

func (g *GitRepo) WriteArchive(w io.Writer, format string, prefix string) error {
	args := []string{"archive", "--format=" + format}
	if prefix != "" {
		args = append(args, "--prefix="+strings.TrimRight(prefix, "/")+"/")
	}
	args = append(args, g.h.String())

	cmd := exec.Command("git", args...)
	cmd.Dir = g.path
	cmd.Stdout = w
	stderr := new(bytes.Buffer)
	cmd.Stderr = stderr

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%w, stderr: %s", err, stderr.String())
	}

	return nil
}
