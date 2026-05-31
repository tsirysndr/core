package gitea

import (
	"context"
	"errors"
	"fmt"
	"os/exec"
	"strings"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
)

func GetCommitByPathWithID(ctx context.Context, oid plumbing.Hash, repoPath, relpath string) (*object.Commit, error) {
	if len(relpath) == 0 {
		return nil, errors.New("relpath should not be empty")
	}
	// File name starts with ':' must be escaped.
	if relpath[0] == ':' {
		relpath = `\` + relpath
	}

	out, err := exec.CommandContext(ctx,
		"git",
		"-C", repoPath,
		"log",
		"-1",
		"--pretty=format:%H",
		oid.String(),
		"--", relpath,
	).Output()
	if err != nil {
		return nil, err
	}

	rev := plumbing.NewHash(strings.TrimSpace(string(out)))
	if rev.IsZero() {
		return nil, fmt.Errorf("invalid commit id: %q", string(out))
	}

	return GetCommit(ctx, repoPath, rev.String())
}
