package gitea

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func initRepo(t *testing.T) (string, func(args ...string) []byte) {
	t.Helper()
	repo := t.TempDir()
	runGit := func(args ...string) []byte {
		t.Helper()
		cmd := exec.Command("git", args...)
		cmd.Dir = repo
		out, err := cmd.CombinedOutput()
		require.NoError(t, err, string(out))
		return out
	}
	runGit("init")
	runGit("config", "user.name", "nel")
	runGit("config", "user.email", "nel@oyster.cafe")
	runGit("config", "maintenance.auto", "false")
	runGit("config", "gc.autoDetach", "false")
	return repo, runGit
}

func sizesByName(t *testing.T, repo string) map[string]int64 {
	t.Helper()
	ctx := context.Background()
	tree, err := GetTree(ctx, repo, "HEAD^{tree}")
	require.NoError(t, err)

	sizes, err := EntrySizes(ctx, repo, tree.Entries)
	require.NoError(t, err, "the batch survives an entry pointing outside this repo")
	require.Len(t, sizes, len(tree.Entries))

	byName := map[string]int64{}
	for i, entry := range tree.Entries {
		byName[entry.Name] = sizes[i]
	}
	return byName
}

func TestEntrySizesReportsEveryEntryKind(t *testing.T) {
	repo, runGit := initRepo(t)
	require.NoError(t, os.WriteFile(filepath.Join(repo, "conch.txt"), make([]byte, 10), 0o644))
	require.NoError(t, os.WriteFile(filepath.Join(repo, "kelp.txt"), make([]byte, 9000), 0o644))
	require.NoError(t, os.Mkdir(filepath.Join(repo, "limpet"), 0o755))
	require.NoError(t, os.WriteFile(filepath.Join(repo, "limpet", "uni.txt"), make([]byte, 3), 0o644))
	require.NoError(t, os.Symlink("conch.txt", filepath.Join(repo, "cuttle.txt")))
	runGit("add", ".")
	runGit("update-index", "--add", "--cacheinfo",
		"160000,0000000000000000000000000000000000000001,mussel")
	require.NoError(t, os.WriteFile(filepath.Join(repo, "zzz-nautilus.txt"), make([]byte, 4096), 0o644))
	runGit("add", "zzz-nautilus.txt")
	runGit("commit", "-m", "initial")

	byName := sizesByName(t, repo)
	assert.Equal(t, int64(10), byName["conch.txt"])
	assert.Equal(t, int64(9000), byName["kelp.txt"])
	assert.Equal(t, int64(0), byName["limpet"], "a tree doesn't have a blob size")
	assert.Equal(t, int64(len("conch.txt")), byName["cuttle.txt"], "a symlink is its target's length")
	assert.Equal(t, int64(0), byName["mussel"], "a submodule doesn't have a blob size")
	assert.Equal(t, int64(4096), byName["zzz-nautilus.txt"], "entries after a submodule keep their size")

	sizes, err := EntrySizes(context.Background(), repo, nil)
	require.NoError(t, err)
	assert.Empty(t, sizes, "we don't spawn a git process for an empty tree")
}

func TestEntrySizesReadsATreeLargerThanThePipeBuffer(t *testing.T) {
	repo, runGit := initRepo(t)
	const entries = 2000
	for i := range entries {
		name := fmt.Sprintf("scallop-%04d.txt", i)
		require.NoError(t, os.WriteFile(filepath.Join(repo, name), make([]byte, i), 0o644))
	}
	runGit("add", ".")
	runGit("commit", "-m", "initial")

	byName := sizesByName(t, repo)
	require.Len(t, byName, entries)
	for i := range entries {
		name := fmt.Sprintf("scallop-%04d.txt", i)
		require.Equal(t, int64(i), byName[name], name)
	}
}
