package compat113

import (
	"encoding/json"
	"testing"

	"tangled.org/core/api/tangled"
)

func ptr[T any](v T) *T { return &v }

func TestCollaboratorShadowsRepoDid(t *testing.T) {
	rec := &tangled.RepoCollaborator{
		CreatedAt: "2026-05-08T00:00:00Z",
		Repo:      "did:plc:abalone",
		Subject:   "did:plc:limpet",
	}

	out, err := json.Marshal(Collaborator(rec))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if got["$type"] != "sh.tangled.repo.collaborator" {
		t.Errorf("$type = %v, want sh.tangled.repo.collaborator", got["$type"])
	}
	if got["repo"] != "did:plc:abalone" {
		t.Errorf("repo = %v, want did:plc:abalone", got["repo"])
	}
	if got["repoDid"] != "did:plc:abalone" {
		t.Errorf("repoDid shadow missing or wrong: got %v", got["repoDid"])
	}
}

func TestPullShadowsTargetRepoDid(t *testing.T) {
	rec := &tangled.RepoPull{
		CreatedAt: "2026-05-08T00:00:00Z",
		Title:     "rename whelk handler",
		Target: &tangled.RepoPull_Target{
			Branch: "main",
			Repo:   "did:plc:scallop",
		},
		Source: &tangled.RepoPull_Source{
			Branch: "feature-1",
		},
	}

	out, err := json.Marshal(Pull(rec))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	target, ok := got["target"].(map[string]any)
	if !ok {
		t.Fatalf("target missing or wrong type: %v", got["target"])
	}
	if target["repo"] != "did:plc:scallop" {
		t.Errorf("target.repo = %v", target["repo"])
	}
	if target["repoDid"] != "did:plc:scallop" {
		t.Errorf("target.repoDid shadow missing: %v", target["repoDid"])
	}

	if _, has := got["repoDid"]; has {
		t.Errorf("top-level repoDid should not be set on pull: %v", got["repoDid"])
	}
}

func TestPullShadowsForkSourceRepoDid(t *testing.T) {
	rec := &tangled.RepoPull{
		CreatedAt: "2026-05-08T00:00:00Z",
		Title:     "fork-based PR",
		Target: &tangled.RepoPull_Target{
			Branch: "main",
			Repo:   "did:plc:scallop",
		},
		Source: &tangled.RepoPull_Source{
			Branch: "feature-2",
			Repo:   ptr("did:plc:periwinkle"),
		},
	}

	out, err := json.Marshal(Pull(rec))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	source, ok := got["source"].(map[string]any)
	if !ok {
		t.Fatalf("source missing: %v", got["source"])
	}
	if source["repo"] != "did:plc:periwinkle" {
		t.Errorf("source.repo = %v", source["repo"])
	}
	if source["repoDid"] != "did:plc:periwinkle" {
		t.Errorf("source.repoDid shadow missing: %v", source["repoDid"])
	}
}
