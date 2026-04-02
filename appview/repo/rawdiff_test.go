package repo

import (
	"strings"
	"testing"
	"time"

	"github.com/bluekeyes/go-gitdiff/gitdiff"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/types"
)

// parseDiff is a test helper that parses a unified diff string into gitdiff.TextFragment slices.
func parseDiff(t *testing.T, src string) []*gitdiff.File {
	t.Helper()
	files, _, err := gitdiff.Parse(strings.NewReader(src))
	if err != nil {
		t.Fatalf("gitdiff.Parse: %v", err)
	}
	return files
}

// niceDiffFromParsed builds a NiceDiff from parsed gitdiff.File entries, mirroring
// what the knotserver does in git.Diff().
func niceDiffFromParsed(files []*gitdiff.File, commit types.Commit, stat types.DiffStat) *types.NiceDiff {
	nd := &types.NiceDiff{Commit: commit, Stat: stat}
	for _, f := range files {
		d := types.Diff{
			IsBinary: f.IsBinary,
			IsNew:    f.IsNew,
			IsDelete: f.IsDelete,
			IsCopy:   f.IsCopy,
			IsRename: f.IsRename,
		}
		d.Name.Old = f.OldName
		d.Name.New = f.NewName
		for _, tf := range f.TextFragments {
			d.TextFragments = append(d.TextFragments, *tf)
		}
		nd.Diff = append(nd.Diff, d)
	}
	return nd
}

func TestRenderUnifiedDiff_nil(t *testing.T) {
	if got := renderUnifiedDiff(nil); got != "" {
		t.Errorf("expected empty string for nil NiceDiff, got %q", got)
	}
}

func TestRenderUnifiedDiff_modified(t *testing.T) {
	const src = `diff --git a/foo.go b/foo.go
--- a/foo.go
+++ b/foo.go
@@ -1,3 +1,3 @@
 package main
-// old comment
+// new comment
 func main() {}
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{}, types.DiffStat{})
	got := renderUnifiedDiff(nd)

	checks := []string{
		"diff --git a/foo.go b/foo.go\n",
		"--- a/foo.go\n",
		"+++ b/foo.go\n",
		"@@ -1,3 +1,3 @@",
		"-// old comment\n",
		"+// new comment\n",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderUnifiedDiff output missing %q\ngot:\n%s", want, got)
		}
	}
}

func TestRenderUnifiedDiff_newFile(t *testing.T) {
	const src = `diff --git a/new.go b/new.go
new file mode 100644
--- /dev/null
+++ b/new.go
@@ -0,0 +1,2 @@
+package main
+func main() {}
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{}, types.DiffStat{})
	got := renderUnifiedDiff(nd)

	checks := []string{
		"diff --git a/new.go b/new.go\n",
		"new file mode 100644\n",
		"--- /dev/null\n",
		"+++ b/new.go\n",
		"+package main\n",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderUnifiedDiff output missing %q\ngot:\n%s", want, got)
		}
	}
}

func TestRenderUnifiedDiff_deletedFile(t *testing.T) {
	const src = `diff --git a/old.go b/old.go
deleted file mode 100644
--- a/old.go
+++ /dev/null
@@ -1,2 +0,0 @@
-package main
-func main() {}
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{}, types.DiffStat{})
	got := renderUnifiedDiff(nd)

	checks := []string{
		"diff --git a/old.go b/old.go\n",
		"deleted file mode 100644\n",
		"--- a/old.go\n",
		"+++ /dev/null\n",
		"-package main\n",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderUnifiedDiff output missing %q\ngot:\n%s", want, got)
		}
	}
}

func TestRenderUnifiedDiff_renamedFile(t *testing.T) {
	const src = `diff --git a/old.go b/renamed.go
rename from old.go
rename to renamed.go
--- a/old.go
+++ b/renamed.go
@@ -1,2 +1,2 @@
 package main
-func old() {}
+func renamed() {}
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{}, types.DiffStat{})
	got := renderUnifiedDiff(nd)

	checks := []string{
		"diff --git a/old.go b/renamed.go\n",
		"rename from old.go\n",
		"rename to renamed.go\n",
		"--- a/old.go\n",
		"+++ b/renamed.go\n",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderUnifiedDiff output missing %q\ngot:\n%s", want, got)
		}
	}
}

func TestRenderUnifiedDiff_multipleFiles(t *testing.T) {
	const src = `diff --git a/a.go b/a.go
--- a/a.go
+++ b/a.go
@@ -1,1 +1,1 @@
-old a
+new a
diff --git a/b.go b/b.go
--- a/b.go
+++ b/b.go
@@ -1,1 +1,1 @@
-old b
+new b
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{}, types.DiffStat{})
	got := renderUnifiedDiff(nd)

	for _, want := range []string{"diff --git a/a.go b/a.go", "diff --git a/b.go b/b.go"} {
		if !strings.Contains(got, want) {
			t.Errorf("missing %q in output:\n%s", want, got)
		}
	}
}

func TestRenderFormatPatch_nil(t *testing.T) {
	if got := renderFormatPatch(nil); got != "" {
		t.Errorf("expected empty string for nil NiceDiff, got %q", got)
	}
}

func TestRenderFormatPatch_headers(t *testing.T) {
	when := time.Date(2024, 3, 15, 10, 30, 0, 0, time.UTC)
	hash := plumbing.NewHash("abc1234567890000000000000000000000000000")

	nd := &types.NiceDiff{
		Commit: types.Commit{
			Hash:    hash,
			Message: "Fix the bug\n\nThis patch resolves the long-standing issue.\n",
			Author: object.Signature{
				Name:  "Alice Dev",
				Email: "alice@example.com",
				When:  when,
			},
		},
		Stat: types.DiffStat{FilesChanged: 1, Insertions: 2, Deletions: 1},
	}

	got := renderFormatPatch(nd)

	checks := []string{
		"From abc1234567890000000000000000000000000000 Mon Sep 17 00:00:00 2001\n",
		"From: Alice Dev <alice@example.com>\n",
		"Date: Fri, 15 Mar 2024 10:30:00 +0000\n",
		"Subject: [PATCH] Fix the bug\n",
		"This patch resolves the long-standing issue.\n",
		"---\n",
		" 1 file(s) changed, 2 insertion(s)(+), 1 deletion(s)(-)\n",
		"\n--\ntangled.sh\n",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderFormatPatch output missing %q\ngot:\n%s", want, got)
		}
	}
}

func TestRenderFormatPatch_subjectOnly(t *testing.T) {
	// Single-line message (no body) should not emit a blank body section.
	nd := &types.NiceDiff{
		Commit: types.Commit{
			Message: "Single line commit",
			Author:  object.Signature{When: time.Now()},
		},
	}
	got := renderFormatPatch(nd)

	if !strings.Contains(got, "Subject: [PATCH] Single line commit\n") {
		t.Errorf("missing subject in output:\n%s", got)
	}
	// Body should not appear between Subject and "---"
	parts := strings.SplitN(got, "---\n", 2)
	if len(parts) < 2 {
		t.Fatalf("expected '---' separator in output:\n%s", got)
	}
	beforeSep := parts[0]
	// Only the blank line between headers and body should be there, no extra content.
	afterSubject := strings.SplitN(beforeSep, "Subject: [PATCH] Single line commit\n", 2)
	if len(afterSubject) == 2 && strings.TrimSpace(afterSubject[1]) != "" {
		t.Errorf("unexpected body content before '---': %q", afterSubject[1])
	}
}

func TestRenderFormatPatch_containsDiff(t *testing.T) {
	const src = `diff --git a/foo.go b/foo.go
--- a/foo.go
+++ b/foo.go
@@ -1,2 +1,2 @@
 package main
-// old
+// new
`
	files := parseDiff(t, src)
	nd := niceDiffFromParsed(files, types.Commit{
		Author: object.Signature{When: time.Now()},
	}, types.DiffStat{FilesChanged: 1, Insertions: 1, Deletions: 1})

	got := renderFormatPatch(nd)

	checks := []string{
		"diff --git a/foo.go b/foo.go\n",
		"-// old\n",
		"+// new\n",
		" foo.go |",
	}
	for _, want := range checks {
		if !strings.Contains(got, want) {
			t.Errorf("renderFormatPatch output missing %q\ngot:\n%s", want, got)
		}
	}
}
