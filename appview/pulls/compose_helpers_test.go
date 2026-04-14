package pulls

import (
	"io"
	"log/slog"
	"net/url"
	"reflect"
	"testing"
	"time"

	"github.com/go-git/go-git/v5/plumbing/object"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/appview/validator"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"
)

func TestBracketComponents(t *testing.T) {
	cases := []struct {
		key, prefix string
		want        []string
		ok          bool
	}{
		{"foo[a]", "foo", []string{"a"}, true},
		{"foo[a][b]", "foo", []string{"a", "b"}, true},
		{"foo[a][b][c]", "foo", []string{"a", "b", "c"}, true},
		{"foo[]", "foo", []string{""}, true},
		{"foo[a][]", "foo", []string{"a", ""}, true},
		{"foo", "foo", nil, false},
		{"bar[a]", "foo", nil, false},
		{"foo[a", "foo", nil, false},
		{"fooa]", "foo", nil, false},
		{"foo[a]extra", "foo", nil, false},
		{"", "foo", nil, false},
	}
	for _, c := range cases {
		got, ok := bracketComponents(c.key, c.prefix)
		if ok != c.ok || !reflect.DeepEqual(got, c.want) {
			t.Errorf("bracketComponents(%q, %q) = %v, %v; want %v, %v", c.key, c.prefix, got, ok, c.want, c.ok)
		}
	}
}

func TestParseBracketedForm(t *testing.T) {
	form := url.Values{
		"stackTitle[abc]":   {"hello"},
		"stackTitle[xyz]":   {"world", "ignored"},
		"stackTitle[]":      {"empty-id"},
		"stackTitle[a][b]":  {"too-deep"},
		"stackTitle":        {"no-bracket"},
		"unrelated[abc]":    {"skip"},
		"stackTitle[noval]": {},
	}
	got := parseBracketedForm(form, "stackTitle")
	want := map[string]string{
		"abc": "hello",
		"xyz": "world",
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("parseBracketedForm = %v; want %v", got, want)
	}
}

func TestParseStackLabelForms(t *testing.T) {
	form := url.Values{
		"stackLabel[c1][at://uri/a]": {"v1"},
		"stackLabel[c1][at://uri/b]": {"v2"},
		"stackLabel[c2][at://uri/a]": {"v3", "v4"},
		"stackLabel[c1][]":           {"empty-uri"},
		"stackLabel[][at://uri/a]":   {"empty-cid"},
		"stackLabel[c1]":             {"missing-second-bracket"},
		"stackLabel[c1][a][b]":       {"too-deep"},
		"stackTitle[c1]":             {"wrong-prefix"},
	}
	got := parseStackLabelForms(form)
	want := map[string]url.Values{
		"c1": {
			"at://uri/a": {"v1"},
			"at://uri/b": {"v2"},
		},
		"c2": {
			"at://uri/a": {"v3", "v4"},
		},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("parseStackLabelForms = %v; want %v", got, want)
	}
}

func TestDefaultTargetBranch(t *testing.T) {
	branches := []types.Branch{
		{Reference: types.Reference{Name: "feature"}},
		{Reference: types.Reference{Name: "main"}, IsDefault: true},
	}
	cases := []struct {
		name     string
		branches []types.Branch
		current  string
		want     string
	}{
		{"current is valid", branches, "feature", "feature"},
		{"current is default", branches, "main", "main"},
		{"current invalid, falls to default", branches, "ghost", "main"},
		{"current empty, falls to default", branches, "", "main"},
		{"no default, no match returns empty", []types.Branch{{Reference: types.Reference{Name: "only"}}}, "ghost", ""},
		{"empty branches returns empty", nil, "anything", ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := defaultTargetBranch(c.branches, c.current); got != c.want {
				t.Errorf("defaultTargetBranch = %q; want %q", got, c.want)
			}
		})
	}
}

func TestDefaultSourceBranch(t *testing.T) {
	choices := []types.Branch{
		{Reference: types.Reference{Name: "feature"}},
		{Reference: types.Reference{Name: "wip"}},
	}
	forks := []types.Branch{
		{Reference: types.Reference{Name: "fork-feature"}},
	}
	cases := []struct {
		name    string
		source  pages.Source
		current string
		want    string
	}{
		{"branch source, valid current", pages.SourceBranch, "feature", "feature"},
		{"branch source, invalid falls to first", pages.SourceBranch, "ghost", "feature"},
		{"branch source, empty falls to first", pages.SourceBranch, "", "feature"},
		{"fork source, valid current", pages.SourceFork, "fork-feature", "fork-feature"},
		{"fork source, invalid falls to first fork", pages.SourceFork, "ghost", "fork-feature"},
		{"patch source preserves current", pages.SourcePatch, "anything", "anything"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := defaultSourceBranch(c.source, c.current, choices, forks); got != c.want {
				t.Errorf("defaultSourceBranch = %q; want %q", got, c.want)
			}
		})
	}
	if got := defaultSourceBranch(pages.SourceBranch, "", nil, nil); got != "" {
		t.Errorf("empty choices should return empty, got %q", got)
	}
}

func TestSortBranchesByRecency(t *testing.T) {
	mk := func(name string, when *time.Time) types.Branch {
		b := types.Branch{Reference: types.Reference{Name: name}}
		if when != nil {
			b.Commit = &object.Commit{Committer: object.Signature{When: *when}}
		}
		return b
	}
	t1 := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	t2 := time.Date(2026, 2, 1, 0, 0, 0, 0, time.UTC)
	t3 := time.Date(2026, 3, 1, 0, 0, 0, 0, time.UTC)

	in := []types.Branch{
		mk("oldest", &t1),
		mk("newest", &t3),
		mk("nil-commit", nil),
		mk("middle", &t2),
	}
	got := sortBranchesByRecency(in)
	wantNames := []string{"newest", "middle", "oldest", "nil-commit"}
	for i, want := range wantNames {
		if got[i].Reference.Name != want {
			t.Errorf("position %d: got %q, want %q", i, got[i].Reference.Name, want)
		}
	}

	if &got[0] == &in[0] {
		t.Error("expected new slice, got aliased input")
	}
}

func TestComposeCanonicalURL(t *testing.T) {
	repo := repoinfo.RepoInfo{OwnerDid: "did:plc:abc", Name: "demo", Rkey: "demo"}
	cases := []struct {
		name string
		p    pages.RepoNewPullParams
		want string
	}{
		{
			"defaults",
			pages.RepoNewPullParams{RepoInfo: repo, Source: pages.SourceBranch},
			"/did:plc:abc/demo/pulls/new",
		},
		{
			"stacked",
			pages.RepoNewPullParams{RepoInfo: repo, Source: pages.SourceBranch, IsStacked: true},
			"/did:plc:abc/demo/pulls/new?mode=stack",
		},
		{
			"fork with selection",
			pages.RepoNewPullParams{
				RepoInfo:     repo,
				Source:       pages.SourceFork,
				Fork:         "did:plc:other/repo",
				SourceBranch: "feature",
				TargetBranch: "main",
			},
			"/did:plc:abc/demo/pulls/new?fork=did%3Aplc%3Aother%2Frepo&source=fork&sourceBranch=feature&targetBranch=main",
		},
		{
			"branch with selection drops source param",
			pages.RepoNewPullParams{
				RepoInfo:     repo,
				Source:       pages.SourceBranch,
				SourceBranch: "feature",
				TargetBranch: "main",
			},
			"/did:plc:abc/demo/pulls/new?sourceBranch=feature&targetBranch=main",
		},
		{
			"fork field skipped when source != fork",
			pages.RepoNewPullParams{
				RepoInfo: repo,
				Source:   pages.SourceBranch,
				Fork:     "stale",
			},
			"/did:plc:abc/demo/pulls/new",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := composeCanonicalURL(c.p); got != c.want {
				t.Errorf("composeCanonicalURL = %q; want %q", got, c.want)
			}
		})
	}
}

func TestLabelStateFromForm(t *testing.T) {
	bug := &models.LabelDefinition{
		Did: "did:plc:test", Rkey: "bug", Name: "bug",
		ValueType: models.ValueType{Type: models.ConcreteTypeNull},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	priority := &models.LabelDefinition{
		Did: "did:plc:test", Rkey: "priority", Name: "priority",
		ValueType: models.ValueType{Type: models.ConcreteTypeString, Enum: []string{"low", "med", "high"}},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	defs := map[string]*models.LabelDefinition{
		bug.AtUri().String():      bug,
		priority.AtUri().String(): priority,
	}

	form := url.Values{
		bug.AtUri().String():      {"null"},
		priority.AtUri().String(): {"high", ""},
		"unrelated":               {"ignored"},
	}
	state := labelStateFromForm(form, defs)
	if !state.ContainsLabel(bug.AtUri().String()) {
		t.Error("expected bug label in state")
	}
	if !state.ContainsLabel(priority.AtUri().String()) {
		t.Error("expected priority label in state")
	}

	emptyState := labelStateFromForm(url.Values{}, defs)
	if emptyState.ContainsLabel(bug.AtUri().String()) {
		t.Error("empty form should produce empty state")
	}
}

func TestStackPerCommitDiffs(t *testing.T) {
	if got := stackPerCommitDiffs(nil, "main", "", nil); got != nil {
		t.Errorf("nil comparison should return nil, got %v", got)
	}

	formatPatch := `From 1111111111111111111111111111111111111111 Mon Sep 11 00:00:00 2001
From: Test <t@e.st>
Date: Tue, 1 Jan 2020 00:00:00 +0000
Subject: [PATCH] one
Change-Id: Iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

---
 a.txt | 1 +
 1 file changed, 1 insertion(+)

diff --git a/a.txt b/a.txt
index 0000000..1111111 100644
--- a/a.txt
+++ b/a.txt
@@ -0,0 +1 @@
+hello
`
	patches, err := patchutil.ExtractPatches(formatPatch)
	if err != nil {
		t.Fatalf("extract: %v", err)
	}
	if len(patches) != 1 {
		t.Fatalf("expected 1 patch, got %d", len(patches))
	}
	if cid, err := patches[0].ChangeId(); err != nil || cid == "" {
		t.Fatalf("change-id missing from fixture: %v %q", err, cid)
	}
	comp := &types.RepoFormatPatchResponse{
		FormatPatchRaw: formatPatch,
		FormatPatch:    patches,
	}

	got := stackPerCommitDiffs(comp, "main", "/repo/pulls/new/refresh", map[string]string{
		"Iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": "split",
	})
	if len(got) != 1 {
		t.Fatalf("expected 1 entry, got %d", len(got))
	}
	if got[0].Diff == nil {
		t.Error("Diff should be set")
	}
	if !got[0].Opts.Split {
		t.Error("Split should propagate from stackSplits")
	}
	if got[0].Opts.RefreshUrl != "/repo/pulls/new/refresh" {
		t.Errorf("RefreshUrl: got %q", got[0].Opts.RefreshUrl)
	}
	if got[0].Opts.Target != "#stack-diff-Iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
		t.Errorf("Target: got %q", got[0].Opts.Target)
	}
	if got[0].Opts.Field != "stackSplit[Iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]" {
		t.Errorf("Field: got %q", got[0].Opts.Field)
	}
}

func TestStackPerCommitDiffsNoChangeId(t *testing.T) {
	formatPatch := `From 1111111111111111111111111111111111111111 Mon Sep 11 00:00:00 2001
From: Test <t@e.st>
Date: Tue, 1 Jan 2020 00:00:00 +0000
Subject: [PATCH] no-cid

---
 a.txt | 1 +
 1 file changed, 1 insertion(+)

diff --git a/a.txt b/a.txt
index 0000000..1111111 100644
--- a/a.txt
+++ b/a.txt
@@ -0,0 +1 @@
+hello
`
	patches, err := patchutil.ExtractPatches(formatPatch)
	if err != nil {
		t.Fatalf("extract: %v", err)
	}
	comp := &types.RepoFormatPatchResponse{
		FormatPatchRaw: formatPatch,
		FormatPatch:    patches,
	}
	got := stackPerCommitDiffs(comp, "main", "/r", nil)
	if len(got) != 1 {
		t.Fatalf("len: %d", len(got))
	}
	if got[0].Diff == nil {
		t.Error("Diff still set even without change-id")
	}
	if got[0].Opts != (types.DiffOpts{}) {
		t.Errorf("Opts should be zero without change-id, got %+v", got[0].Opts)
	}
}

func TestPrefetchComparisonPatch(t *testing.T) {
	s := &Pulls{
		validator: &validator.Validator{},
		logger:    slog.New(slog.NewTextHandler(io.Discard, nil)),
	}

	cases := []struct {
		name    string
		patch   string
		wantNil bool
		wantErr bool
	}{
		{"empty patch returns nil", "", true, false},
		{"whitespace patch returns nil", "   \n  ", true, false},
		{"garbage patch errors", "not a patch", false, true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			comp, diff, err := s.prefetchComparison(nil, nil, pages.SourcePatch, "", "", "", c.patch)
			if c.wantErr {
				if err == nil {
					t.Fatal("expected error")
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if c.wantNil {
				if comp != nil || diff != nil {
					t.Errorf("expected nil, got comp=%v diff=%v", comp, diff)
				}
			}
		})
	}
}

func TestPrefetchComparisonValidPatch(t *testing.T) {
	s := &Pulls{
		validator: &validator.Validator{},
		logger:    slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	patch := `diff --git a/a.txt b/a.txt
index 0000000..1111111 100644
--- a/a.txt
+++ b/a.txt
@@ -0,0 +1 @@
+hello
`
	comp, diff, err := s.prefetchComparison(nil, nil, pages.SourcePatch, "", "main", "", patch)
	if err != nil {
		t.Fatalf("err: %v", err)
	}
	if comp == nil {
		t.Fatal("comp nil")
	}
	if comp.FormatPatchRaw == "" {
		t.Error("FormatPatchRaw empty")
	}
	if diff == nil {
		t.Error("diff nil")
	}
}

func TestPrefetchComparisonMissingInputs(t *testing.T) {
	s := &Pulls{
		validator: &validator.Validator{},
		logger:    slog.New(slog.NewTextHandler(io.Discard, nil)),
	}

	cases := []struct {
		name         string
		source       pages.Source
		fork         string
		targetBranch string
		sourceBranch string
	}{
		{"branch missing target", pages.SourceBranch, "", "", "feature"},
		{"branch missing source", pages.SourceBranch, "", "main", ""},
		{"fork missing fork", pages.SourceFork, "", "main", "feature"},
		{"fork missing target", pages.SourceFork, "did/repo", "", "feature"},
		{"fork missing source", pages.SourceFork, "did/repo", "main", ""},
		{"unknown source", pages.Source("bogus"), "", "", ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			comp, diff, err := s.prefetchComparison(nil, nil, c.source, c.fork, c.targetBranch, c.sourceBranch, "")
			if err != nil {
				t.Errorf("expected nil err, got %v", err)
			}
			if comp != nil || diff != nil {
				t.Errorf("expected nil result, got comp=%v diff=%v", comp, diff)
			}
		})
	}
}
