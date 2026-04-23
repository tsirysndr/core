package pages

import (
	"bytes"
	"io"
	"log/slog"
	"strings"
	"testing"

	"tangled.org/core/appview/config"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"
)

func TestPullComposeTemplatesParse(t *testing.T) {
	cfg := &config.Config{}
	p := NewPages(cfg, nil, nil, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))

	cases := []struct {
		name  string
		stack []string
	}{
		{"new.html via repo base", []string{"layouts/base", "layouts/repobase", "repo/pulls/new"}},
		{"pullComposeHost", []string{"repo/pulls/fragments/pullComposeHost"}},
		{"pullStepSource", []string{"repo/pulls/fragments/pullStepSource"}},
		{"pullStepReview", []string{"repo/pulls/fragments/pullStepReview"}},
		{"pullStepDetails", []string{"repo/pulls/fragments/pullStepDetails"}},
		{"pullCompareForks", []string{"repo/pulls/fragments/pullCompareForks"}},
		{"pullCompareBranches", []string{"repo/pulls/fragments/pullCompareBranches"}},
		{"pullCompareForksBranches", []string{"repo/pulls/fragments/pullCompareForksBranches"}},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if _, err := p.rawParse(c.stack...); err != nil {
				t.Fatalf("parse %v: %v", c.stack, err)
			}
		})
	}
}

func TestPullComposeHostRender(t *testing.T) {
	cfg := &config.Config{}
	p := NewPages(cfg, nil, nil, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))

	base := RepoNewPullParams{
		RepoInfo: repoinfo.RepoInfo{
			OwnerDid: "did:plc:test",
			Name:     "test-repo",
		},
	}

	for _, source := range []Source{"", SourceBranch, SourceFork, SourcePatch} {
		for _, stacked := range []bool{false, true} {
			if source == SourcePatch && stacked {
				continue
			}
			params := base
			params.Source = source
			params.IsStacked = stacked
			name := string(source)
			if name == "" {
				name = "default"
			}
			if stacked {
				name += "-stacked"
			}
			t.Run(name, func(t *testing.T) {
				if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
					t.Fatalf("render source=%q stacked=%v: %v", source, stacked, err)
				}
			})
		}
	}
}

func TestPullComposeHostRenderWithData(t *testing.T) {
	cfg := &config.Config{}
	p := NewPages(cfg, nil, nil, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))

	sampleBranches := []types.Branch{
		{Reference: types.Reference{Name: "feature"}},
		{Reference: types.Reference{Name: "main"}, IsDefault: true},
	}

	formatPatch := `From 1111111111111111111111111111111111111111 Mon Sep 11 00:00:00 2001
From: Test <test@best.fest>
Date: Tue, 1 Jan 2020 00:00:00 +0000
Subject: [PATCH] example commit

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
		t.Fatalf("extract patches: %v", err)
	}
	comparison := &types.RepoFormatPatchResponse{
		FormatPatchRaw: formatPatch,
		FormatPatch:    patches,
	}
	diff := patchutil.AsNiceDiff(formatPatch, "main")

	params := RepoNewPullParams{
		RepoInfo: repoinfo.RepoInfo{
			OwnerDid: "did:plc:test",
			Name:     "test-repo",
		},
		Branches:       sampleBranches,
		SourceBranches: []types.Branch{sampleBranches[0]},
		ForkBranches:   []types.Branch{sampleBranches[0]},
		Source:         SourceBranch,
		SourceBranch:   "feature",
		TargetBranch:   "main",
		Comparison:     comparison,
		Diff:           &diff,
	}

	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render with data: %v", err)
	}

	params.IsStacked = true
	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render stacked: %v", err)
	}

	params.PrefillError = "branch not found"
	params.Comparison = nil
	params.Diff = nil
	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render with prefill error: %v", err)
	}

	bugDef := &models.LabelDefinition{
		Did:       "did:plc:test",
		Rkey:      "bug",
		Name:      "bug",
		ValueType: models.ValueType{Type: models.ConcreteTypeNull},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	priorityDef := &models.LabelDefinition{
		Did:       "did:plc:test",
		Rkey:      "priority",
		Name:      "priority",
		ValueType: models.ValueType{Type: models.ConcreteTypeString, Enum: []string{"low", "med", "high"}},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	assigneeDef := &models.LabelDefinition{
		Did:       "did:plc:test",
		Rkey:      "assignee",
		Name:      "assignee",
		ValueType: models.ValueType{Type: models.ConcreteTypeString, Format: models.ValueTypeFormatDid},
		Scope:     []string{"sh.tangled.repo.pull"},
		Multiple:  true,
	}
	labelDefs := map[string]*models.LabelDefinition{
		bugDef.AtUri().String():      bugDef,
		priorityDef.AtUri().String(): priorityDef,
		assigneeDef.AtUri().String(): assigneeDef,
	}

	pushRepoInfo := repoinfo.RepoInfo{
		OwnerDid: "did:plc:test",
		Name:     "test-repo",
		Roles:    repoinfo.RolesInRepo{Roles: []string{"repo:push"}},
	}
	params = RepoNewPullParams{
		RepoInfo:       pushRepoInfo,
		Branches:       sampleBranches,
		SourceBranches: []types.Branch{sampleBranches[0]},
		Source:         SourceBranch,
		SourceBranch:   "feature",
		TargetBranch:   "main",
		Comparison:     comparison,
		Diff:           &diff,
		LabelDefs:      labelDefs,
		LabelState:     models.NewLabelState(),
	}
	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render with labels: %v", err)
	}

	params.IsStacked = true
	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render stacked with labels: %v", err)
	}

	params.StackedDiffs = []StackedDiff{{
		Diff: &diff,
		Opts: types.DiffOpts{Split: true, RefreshUrl: "/r", Target: "#stack-diff-x", Field: "stackSplit[x]"},
	}}
	if err := p.PullComposeHostFragment(io.Discard, params); err != nil {
		t.Fatalf("render stacked with per-commit diffs: %v", err)
	}
}

func TestPullComposeLabelStateRoundTrip(t *testing.T) {
	cfg := &config.Config{}
	p := NewPages(cfg, nil, nil, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))

	sampleBranches := []types.Branch{
		{Reference: types.Reference{Name: "feature"}},
		{Reference: types.Reference{Name: "main"}, IsDefault: true},
	}

	bugDef := &models.LabelDefinition{
		Did: "did:plc:test", Rkey: "bug", Name: "bug",
		ValueType: models.ValueType{Type: models.ConcreteTypeNull},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	priorityDef := &models.LabelDefinition{
		Did: "did:plc:test", Rkey: "priority", Name: "priority",
		ValueType: models.ValueType{Type: models.ConcreteTypeString, Enum: []string{"low", "med", "high"}},
		Scope:     []string{"sh.tangled.repo.pull"},
	}
	bugKey := bugDef.AtUri().String()
	priorityKey := priorityDef.AtUri().String()
	labelDefs := map[string]*models.LabelDefinition{
		bugKey:      bugDef,
		priorityKey: priorityDef,
	}

	state := models.NewLabelState()
	actx := &models.LabelApplicationCtx{Defs: labelDefs}
	for _, op := range []models.LabelOp{
		{OperandKey: bugKey, OperandValue: "null", Operation: models.LabelOperationAdd},
		{OperandKey: priorityKey, OperandValue: "high", Operation: models.LabelOperationAdd},
	} {
		if err := actx.ApplyLabelOp(state, op); err != nil {
			t.Fatalf("seed state: %v", err)
		}
	}

	formatPatch := `From 1111111111111111111111111111111111111111 Mon Sep 11 00:00:00 2001
From: Test <test@best.fest>
Date: Tue, 1 Jan 2020 00:00:00 +0000
Subject: [PATCH] example commit

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
		t.Fatalf("extract patches: %v", err)
	}
	comparison := &types.RepoFormatPatchResponse{
		FormatPatchRaw: formatPatch,
		FormatPatch:    patches,
	}

	params := RepoNewPullParams{
		RepoInfo: repoinfo.RepoInfo{
			OwnerDid: "did:plc:test",
			Name:     "test-repo",
			Roles:    repoinfo.RolesInRepo{Roles: []string{"repo:push"}},
		},
		Branches:       sampleBranches,
		SourceBranches: []types.Branch{sampleBranches[0]},
		Source:         SourceBranch,
		SourceBranch:   "feature",
		TargetBranch:   "main",
		Comparison:     comparison,
		LabelDefs:      labelDefs,
		LabelState:     state,
	}

	var buf bytes.Buffer
	if err := p.PullComposeHostFragment(&buf, params); err != nil {
		t.Fatalf("render: %v", err)
	}
	out := buf.String()
	for _, want := range []string{
		`value="null" checked`,
		`value="high" checked`,
	} {
		if !strings.Contains(out, want) {
			t.Errorf("missing pre-selection %q", want)
		}
	}
}

func TestParseSource(t *testing.T) {
	cases := []struct {
		in     string
		want   Source
		wantOk bool
	}{
		{"branch", SourceBranch, true},
		{"BRANCH", SourceBranch, true},
		{"fork", SourceFork, true},
		{"patch", SourcePatch, true},
		{"", "", false},
		{"method", "", false},
		{"strategy", "", false},
		{"unknown", "", false},
	}
	for _, c := range cases {
		got, ok := ParseSource(c.in)
		if got != c.want || ok != c.wantOk {
			t.Errorf("ParseSource(%q) = %q, %v; want %q, %v", c.in, got, ok, c.want, c.wantOk)
		}
	}
}
