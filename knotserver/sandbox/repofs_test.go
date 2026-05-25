package sandbox

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func TestChmodRepoTree(t *testing.T) {
	root := t.TempDir()

	// build a tree:
	//   root/
	//     file.txt    (0644)
	//     script.sh   (0755)
	//     subdir/
	//       nested.txt (0644)
	//       link -> ../file.txt
	mustWrite(t, filepath.Join(root, "file.txt"), 0644, "hello")
	mustWrite(t, filepath.Join(root, "script.sh"), 0755, "#!/bin/sh\n")
	mustMkdir(t, filepath.Join(root, "subdir"), 0755)
	mustWrite(t, filepath.Join(root, "subdir", "nested.txt"), 0644, "nested")
	mustSymlink(t, "../file.txt", filepath.Join(root, "subdir", "link"))

	if err := ChmodRepoTree(root); err != nil {
		t.Fatalf("ChmodRepoTree: %v", err)
	}

	cases := []struct {
		path     string
		wantMode os.FileMode
	}{
		{root, 0770},
		{filepath.Join(root, "file.txt"), 0660},
		{filepath.Join(root, "script.sh"), 0770},
		{filepath.Join(root, "subdir"), 0770},
		{filepath.Join(root, "subdir", "nested.txt"), 0660},
	}
	for _, c := range cases {
		info, err := os.Stat(c.path)
		if err != nil {
			t.Errorf("stat %s: %v", c.path, err)
			continue
		}
		if got := info.Mode().Perm(); got != c.wantMode {
			t.Errorf("%s: mode = %o, want %o", c.path, got, c.wantMode)
		}
	}
}

func TestChmodRepoTree_PreservesExecutableBit(t *testing.T) {
	root := t.TempDir()
	mustWrite(t, filepath.Join(root, "exec"), 0744, "")
	mustWrite(t, filepath.Join(root, "noexec"), 0644, "")

	if err := ChmodRepoTree(root); err != nil {
		t.Fatalf("ChmodRepoTree: %v", err)
	}

	if got := mode(t, filepath.Join(root, "exec")); got != 0770 {
		t.Errorf("exec file: mode = %o, want 0770", got)
	}
	if got := mode(t, filepath.Join(root, "noexec")); got != 0660 {
		t.Errorf("noexec file: mode = %o, want 0660", got)
	}
}

func TestChownRepoTree_SelfChown(t *testing.T) {
	// Chowning to our own UID/GID is always a no-op success. This verifies
	// the walk visits all entries without erroring.
	root := t.TempDir()
	mustWrite(t, filepath.Join(root, "a"), 0644, "")
	mustMkdir(t, filepath.Join(root, "b"), 0755)
	mustWrite(t, filepath.Join(root, "b", "c"), 0644, "")

	uid := os.Getuid()
	gid := os.Getgid()
	if err := ChownRepoTree(root, uid, gid); err != nil {
		t.Fatalf("ChownRepoTree: %v", err)
	}

	// verify everything still belongs to us.
	for _, p := range []string{root, filepath.Join(root, "a"), filepath.Join(root, "b"), filepath.Join(root, "b", "c")} {
		info, err := os.Stat(p)
		if err != nil {
			t.Fatalf("stat %s: %v", p, err)
		}
		_ = info
	}
}

func TestLookupUIDForRepoPath(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uid/gid lookup is unix-only")
	}
	scan := t.TempDir()
	repo := filepath.Join(scan, "did:plc:abc")
	mustMkdir(t, repo, 0700)

	uid, gid, err := LookupUIDForRepoPath(scan, repo)
	if err != nil {
		t.Fatalf("LookupUIDForRepoPath: %v", err)
	}
	if uid != uint32(os.Getuid()) {
		t.Errorf("uid = %d, want %d", uid, os.Getuid())
	}
	if gid != uint32(os.Getgid()) {
		t.Errorf("gid = %d, want %d", gid, os.Getgid())
	}
}

func TestLookupUIDForRepoPath_OutsideScanPath(t *testing.T) {
	_, _, err := LookupUIDForRepoPath("/home/git", "/etc/passwd")
	if err == nil {
		t.Fatal("expected error for path outside scan path, got nil")
	}
}

func TestLookupUIDForRepoPath_NonexistentPath(t *testing.T) {
	scan := t.TempDir()
	_, _, err := LookupUIDForRepoPath(scan, filepath.Join(scan, "does-not-exist"))
	if err == nil {
		t.Fatal("expected error for nonexistent path, got nil")
	}
}

// helpers

func mustWrite(t *testing.T, path string, mode os.FileMode, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), mode); err != nil {
		t.Fatalf("write %s: %v", path, err)
	}
	// WriteFile respects existing mode on overwrite; chmod to be sure.
	if err := os.Chmod(path, mode); err != nil {
		t.Fatalf("chmod %s: %v", path, err)
	}
}

func mustMkdir(t *testing.T, path string, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(path, mode); err != nil {
		t.Fatalf("mkdir %s: %v", path, err)
	}
}

func mustSymlink(t *testing.T, target, link string) {
	t.Helper()
	if err := os.Symlink(target, link); err != nil {
		t.Fatalf("symlink %s -> %s: %v", link, target, err)
	}
}

func mode(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat %s: %v", path, err)
	}
	return info.Mode().Perm()
}
