package sandbox

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
)

// ChmodRepoTree sets directory modes to 0770 and file modes to 0660 under
// root, preserving the executable bit on files (hook scripts need it).
// Symlinks are skipped since their mode is not meaningful.
//
// The group bits exist so the knot service (running as the git user, which
// is in the git group that owns the repos) can still read and write the
// repo via group permissions even though the repo's UID owner is a virtual
// UID. Sandbox subprocesses run with NoSetGroups: true so they don't gain
// group access and cross-owner isolation still holds.
func ChmodRepoTree(root string) error {
	return filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.Type()&fs.ModeSymlink != 0 {
			return nil
		}
		if d.IsDir() {
			return os.Chmod(path, 0770)
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		mode := fs.FileMode(0660)
		if info.Mode()&0100 != 0 {
			mode = 0770
		}
		return os.Chmod(path, mode)
	})
}

// ChownRepoTree recursively chowns every entry under root to uid:gid.
// Entries are processed deepest-first so a directory is only chowned after
// its contents, preserving the calling process's access throughout the walk.
// Call ChmodRepoTree first if you also want to tighten permissions; this
// function only changes ownership.
func ChownRepoTree(root string, uid int, gid int) error {
	type entry struct {
		path  string
		depth int
	}
	var entries []entry
	if err := filepath.WalkDir(root, func(path string, _ fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		depth := strings.Count(path, string(filepath.Separator))
		entries = append(entries, entry{path, depth})
		return nil
	}); err != nil {
		return err
	}

	sort.Slice(entries, func(i, j int) bool {
		return entries[i].depth > entries[j].depth
	})

	for _, e := range entries {
		if err := os.Lchown(e.path, uid, gid); err != nil {
			return err
		}
	}
	return nil
}

// LookupUIDForRepoPath returns the owner UID and GID of the repo directory at
// repoPath. scanPath is validated as a prefix to guard against directory escape.
func LookupUIDForRepoPath(scanPath, repoPath string) (uid uint32, gid uint32, err error) {
	if !strings.HasPrefix(repoPath, scanPath) {
		return 0, 0, fmt.Errorf("repo path %q is outside scan path %q", repoPath, scanPath)
	}
	var stat syscall.Stat_t
	if err := syscall.Stat(repoPath, &stat); err != nil {
		return 0, 0, err
	}
	return stat.Uid, stat.Gid, nil
}

// ServiceGid returns the GID of scanPath, which is treated as the "service
// group" that owns all repositories. Callers chown repo trees to
// (virtualUID, ServiceGid(scanPath)) so the knot service (a member of this
// group) retains read+write access via the group bits set by ChmodRepoTree.
func ServiceGid(scanPath string) (uint32, error) {
	var stat syscall.Stat_t
	if err := syscall.Stat(scanPath, &stat); err != nil {
		return 0, fmt.Errorf("stat %s: %w", scanPath, err)
	}
	return stat.Gid, nil
}
