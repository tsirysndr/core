package git

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/assert"
	knotconfig "tangled.org/core/knotserver/config"
)

func TestForkRejectsUnsafeSources(t *testing.T) {
	t.Parallel()

	for _, source := range []string{
		"",
		"--upload-pack=touch /tmp/pwned",
		"-u/bin/sh",
		"ext::sh -c touch% /tmp/pwned",
		"file:///etc/passwd",
		"git://example.com/repo",
		"ssh://example.com/repo",
		"/etc/passwd",
	} {
		t.Run(source, func(t *testing.T) {
			repoPath := filepath.Join(t.TempDir(), "fork")
			err := Fork(repoPath, source, &knotconfig.Config{})
			assert.Error(t, err, "source %q should be rejected", source)
			_, statErr := os.Stat(repoPath)
			assert.True(t, os.IsNotExist(statErr), "source %q must not create a repo", source)
		})
	}
}
