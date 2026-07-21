package git

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
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

func runGit(ctx context.Context, args ...string) error {
	var stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "git", args...)
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%w: %s", err, strings.TrimSpace(stderr.String()))
	}
	return nil
}

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
		if err := runGit(ctx, "clone", "--no-checkout", "--depth=1", "--filter=tree:0", "--revision="+rev, cloneUri, path); err != nil {
			return fmt.Errorf("git clone: %w", err)
		}
		if err := runGit(ctx, "-C", path, "sparse-checkout", "set", "--no-cone", WorkflowDir); err != nil {
			return fmt.Errorf("git sparse-checkout set: %w", err)
		}
	} else {
		if err := runGit(ctx, "-C", path, "fetch", "--depth=1", "--filter=tree:0", "origin", rev); err != nil {
			// remove any locks if the repo was left in a mid fetch state
			removeStaleLocks(path)
			if retryErr := runGit(ctx, "-C", path, "fetch", "--depth=1", "--filter=tree:0", "origin", rev); retryErr != nil {
				// if still broken, wipe and refetch
				if rmErr := os.RemoveAll(path); rmErr != nil {
					return fmt.Errorf("git fetch: %w (cleanup failed: %v)", retryErr, rmErr)
				}
				if cloneErr := runGit(ctx, "clone", "--no-checkout", "--depth=1", "--filter=tree:0", "--revision="+rev, cloneUri, path); cloneErr != nil {
					return fmt.Errorf("git fetch: %w (re-clone failed: %v)", retryErr, cloneErr)
				}
				if cloneErr := runGit(ctx, "-C", path, "sparse-checkout", "set", "--no-cone", WorkflowDir); cloneErr != nil {
					return fmt.Errorf("git sparse-checkout set: %w", cloneErr)
				}
			}
		}
	}
	if err := runGit(ctx, "-C", path, "checkout", rev); err != nil {
		return fmt.Errorf("git checkout: %w", err)
	}
	return nil
}

func removeStaleLocks(path string) {
	// removes shallow.lock, index.lock, etc., all are stale locks
	// worst case scenario we fall through to wipe and refetch anyway
	locks, _ := filepath.Glob(filepath.Join(path, ".git", "*.lock"))
	for _, lock := range locks {
		os.Remove(lock)
	}
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
