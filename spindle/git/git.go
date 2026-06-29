package git

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"

	"github.com/hashicorp/go-version"
)

// repoLocks serializes git operations per repo directory. Concurrent triggers
// on the same repo (a push landing while a manual run is dispatched, two "Run
// CI" clicks, etc.) resolve to the same path with different revisions; running
// clone/fetch/checkout there in parallel collides on .git/index.lock and can
// corrupt the dir. Locking is keyed by path so unrelated repos don't serialize.
var repoLocks keyedMutex

type keyedMutex struct {
	mu sync.Mutex
	m  map[string]*sync.Mutex
}

// lock acquires the mutex for key and returns its unlock func.
func (k *keyedMutex) lock(key string) func() {
	k.mu.Lock()
	if k.m == nil {
		k.m = make(map[string]*sync.Mutex)
	}
	mu, ok := k.m[key]
	if !ok {
		mu = &sync.Mutex{}
		k.m[key] = mu
	}
	k.mu.Unlock()

	mu.Lock()
	return mu.Unlock
}

func Version() (*version.Version, error) {
	var buf bytes.Buffer
	cmd := exec.Command("git", "version")
	cmd.Stdout = &buf
	cmd.Stderr = os.Stderr
	err := cmd.Run()
	if err != nil {
		return nil, err
	}
	fields := strings.Fields(buf.String())
	if len(fields) < 3 {
		return nil, fmt.Errorf("invalid git version: %s", buf.String())
	}

	// version string is like: "git version 2.29.3" or "git version 2.29.3.windows.1"
	versionString := fields[2]
	if pos := strings.Index(versionString, "windows"); pos >= 1 {
		versionString = versionString[:pos-1]
	}
	return version.NewVersion(versionString)
}

const WorkflowDir = `/.tangled/workflows`

func SparseSyncGitRepo(ctx context.Context, cloneUri, path, rev string) error {
	defer repoLocks.lock(path)()

	exist, err := isDir(path)
	if err != nil {
		return err
	}
	if exist {
		gitDirExist, err := isDir(path + "/.git")
		if err != nil {
			return err
		}
		if !gitDirExist {
			if err := os.RemoveAll(path); err != nil {
				return fmt.Errorf("cleanup invalid git dir: %w", err)
			}
			exist = false
		}
	}
	if rev == "" {
		rev = "HEAD"
	}
	if !exist {
		if err := exec.CommandContext(ctx, "git", "clone", "--no-checkout", "--depth=1", "--filter=tree:0", "--revision="+rev, cloneUri, path).Run(); err != nil {
			return fmt.Errorf("git clone: %w", err)
		}
		if err := exec.CommandContext(ctx, "git", "-C", path, "sparse-checkout", "set", "--no-cone", WorkflowDir).Run(); err != nil {
			return fmt.Errorf("git sparse-checkout set: %w", err)
		}
	} else {
		if err := exec.CommandContext(ctx, "git", "-C", path, "fetch", "--depth=1", "--filter=tree:0", "origin", rev).Run(); err != nil {
			return fmt.Errorf("git fetch: %w", err)
		}
	}
	if err := exec.CommandContext(ctx, "git", "-C", path, "checkout", rev).Run(); err != nil {
		return fmt.Errorf("git checkout: %w", err)
	}
	return nil
}

func isDir(path string) (bool, error) {
	info, err := os.Stat(path)
	if err == nil && info.IsDir() {
		return true, nil
	}
	if os.IsNotExist(err) {
		return false, nil
	}
	return false, err
}
