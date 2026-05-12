package models

import (
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/workflow"
)

func TestPipelineEnvVars_PushBranch(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindPush),
		Push: &tangled.Pipeline_PushTriggerData{
			NewSha: "abc123def456",
			OldSha: "000000000000",
			Ref:    "refs/heads/main",
		},
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:          "example.com",
			Did:           "did:plc:user123",
			Repo:          sp("my-repo"),
			RepoDid:       sp("did:plc:boltless"),
			DefaultBranch: "main",
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, false)

	// Check standard CI variable
	if env["CI"] != "true" {
		t.Errorf("Expected CI='true', got '%s'", env["CI"])
	}

	// Check ref variables
	if env["TANGLED_REF"] != "refs/heads/main" {
		t.Errorf("Expected TANGLED_REF='refs/heads/main', got '%s'", env["TANGLED_REF"])
	}
	if env["TANGLED_REF_NAME"] != "main" {
		t.Errorf("Expected TANGLED_REF_NAME='main', got '%s'", env["TANGLED_REF_NAME"])
	}
	if env["TANGLED_REF_TYPE"] != "branch" {
		t.Errorf("Expected TANGLED_REF_TYPE='branch', got '%s'", env["TANGLED_REF_TYPE"])
	}

	// Check SHA variables
	if env["TANGLED_SHA"] != "abc123def456" {
		t.Errorf("Expected TANGLED_SHA='abc123def456', got '%s'", env["TANGLED_SHA"])
	}
	if env["TANGLED_COMMIT_SHA"] != "abc123def456" {
		t.Errorf("Expected TANGLED_COMMIT_SHA='abc123def456', got '%s'", env["TANGLED_COMMIT_SHA"])
	}

	// Check repo variables
	if env["TANGLED_REPO_KNOT"] != "example.com" {
		t.Errorf("Expected TANGLED_REPO_KNOT='example.com', got '%s'", env["TANGLED_REPO_KNOT"])
	}
	if env["TANGLED_REPO_DID"] != "did:plc:user123" {
		t.Errorf("Expected TANGLED_REPO_DID='did:plc:user123', got '%s'", env["TANGLED_REPO_DID"])
	}
	if env["TANGLED_REPO_NAME"] != "my-repo" {
		t.Errorf("Expected TANGLED_REPO_NAME='my-repo', got '%s'", env["TANGLED_REPO_NAME"])
	}
	if env["TANGLED_REPO_DEFAULT_BRANCH"] != "main" {
		t.Errorf("Expected TANGLED_REPO_DEFAULT_BRANCH='main', got '%s'", env["TANGLED_REPO_DEFAULT_BRANCH"])
	}
	if env["TANGLED_REPO_URL"] != "https://example.com/did:plc:boltless" {
		t.Errorf("Expected TANGLED_REPO_URL='https://example.com/did:plc:boltless', got '%s'", env["TANGLED_REPO_URL"])
	}
}

func TestPipelineEnvVars_PushTag(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindPush),
		Push: &tangled.Pipeline_PushTriggerData{
			NewSha: "abc123def456",
			OldSha: "000000000000",
			Ref:    "refs/tags/v1.2.3",
		},
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:    "example.com",
			Did:     "did:plc:user123",
			Repo:    sp("my-repo"),
			RepoDid: sp("did:plc:boltless"),
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, false)

	if env["TANGLED_REF"] != "refs/tags/v1.2.3" {
		t.Errorf("Expected TANGLED_REF='refs/tags/v1.2.3', got '%s'", env["TANGLED_REF"])
	}
	if env["TANGLED_REF_NAME"] != "v1.2.3" {
		t.Errorf("Expected TANGLED_REF_NAME='v1.2.3', got '%s'", env["TANGLED_REF_NAME"])
	}
	if env["TANGLED_REF_TYPE"] != "tag" {
		t.Errorf("Expected TANGLED_REF_TYPE='tag', got '%s'", env["TANGLED_REF_TYPE"])
	}
}

func TestPipelineEnvVars_PullRequest(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindPullRequest),
		PullRequest: &tangled.Pipeline_PullRequestTriggerData{
			SourceBranch: "feature-branch",
			TargetBranch: "main",
			SourceSha:    "pr-sha-789",
			Action:       "opened",
		},
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:    "example.com",
			Did:     "did:plc:user123",
			Repo:    sp("my-repo"),
			RepoDid: sp("did:plc:boltless"),
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, false)

	// Check ref variables for PR
	if env["TANGLED_REF"] != "refs/heads/feature-branch" {
		t.Errorf("Expected TANGLED_REF='refs/heads/feature-branch', got '%s'", env["TANGLED_REF"])
	}
	if env["TANGLED_REF_NAME"] != "feature-branch" {
		t.Errorf("Expected TANGLED_REF_NAME='feature-branch', got '%s'", env["TANGLED_REF_NAME"])
	}
	if env["TANGLED_REF_TYPE"] != "branch" {
		t.Errorf("Expected TANGLED_REF_TYPE='branch', got '%s'", env["TANGLED_REF_TYPE"])
	}

	// Check SHA variables
	if env["TANGLED_SHA"] != "pr-sha-789" {
		t.Errorf("Expected TANGLED_SHA='pr-sha-789', got '%s'", env["TANGLED_SHA"])
	}
	if env["TANGLED_COMMIT_SHA"] != "pr-sha-789" {
		t.Errorf("Expected TANGLED_COMMIT_SHA='pr-sha-789', got '%s'", env["TANGLED_COMMIT_SHA"])
	}

	// Check PR-specific variables
	if env["TANGLED_PR_SOURCE_BRANCH"] != "feature-branch" {
		t.Errorf("Expected TANGLED_PR_SOURCE_BRANCH='feature-branch', got '%s'", env["TANGLED_PR_SOURCE_BRANCH"])
	}
	if env["TANGLED_PR_TARGET_BRANCH"] != "main" {
		t.Errorf("Expected TANGLED_PR_TARGET_BRANCH='main', got '%s'", env["TANGLED_PR_TARGET_BRANCH"])
	}
	if env["TANGLED_PR_SOURCE_SHA"] != "pr-sha-789" {
		t.Errorf("Expected TANGLED_PR_SOURCE_SHA='pr-sha-789', got '%s'", env["TANGLED_PR_SOURCE_SHA"])
	}
	if env["TANGLED_PR_ACTION"] != "opened" {
		t.Errorf("Expected TANGLED_PR_ACTION='opened', got '%s'", env["TANGLED_PR_ACTION"])
	}
}

func TestPipelineEnvVars_ManualWithInputs(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindManual),
		Manual: &tangled.Pipeline_ManualTriggerData{
			Inputs: []*tangled.Pipeline_Pair{
				{Key: "version", Value: "1.0.0"},
				{Key: "environment", Value: "production"},
			},
		},
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:    "example.com",
			Did:     "did:plc:user123",
			Repo:    sp("my-repo"),
			RepoDid: sp("did:plc:boltless"),
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, false)

	// Check manual input variables
	if env["TANGLED_INPUT_VERSION"] != "1.0.0" {
		t.Errorf("Expected TANGLED_INPUT_VERSION='1.0.0', got '%s'", env["TANGLED_INPUT_VERSION"])
	}
	if env["TANGLED_INPUT_ENVIRONMENT"] != "production" {
		t.Errorf("Expected TANGLED_INPUT_ENVIRONMENT='production', got '%s'", env["TANGLED_INPUT_ENVIRONMENT"])
	}

	// Manual triggers shouldn't have ref/sha variables
	if _, ok := env["TANGLED_REF"]; ok {
		t.Error("Manual trigger should not have TANGLED_REF")
	}
	if _, ok := env["TANGLED_SHA"]; ok {
		t.Error("Manual trigger should not have TANGLED_SHA")
	}
}

func TestPipelineEnvVars_DevMode(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindPush),
		Push: &tangled.Pipeline_PushTriggerData{
			NewSha: "abc123",
			Ref:    "refs/heads/main",
		},
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:    "localhost:3000",
			Did:     "did:plc:user123",
			Repo:    sp("my-repo"),
			RepoDid: sp("did:plc:boltless"),
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, true)

	// Dev mode should use http:// and replace localhost with host.docker.internal
	expectedURL := "http://host.docker.internal:3000/did:plc:boltless"
	if env["TANGLED_REPO_URL"] != expectedURL {
		t.Errorf("Expected TANGLED_REPO_URL='%s', got '%s'", expectedURL, env["TANGLED_REPO_URL"])
	}
}

func TestPipelineEnvVars_NilTrigger(t *testing.T) {
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(nil, id, false)

	if env != nil {
		t.Error("Expected nil env for nil trigger")
	}
}

func TestPipelineEnvVars_NilPushData(t *testing.T) {
	tr := &tangled.Pipeline_TriggerMetadata{
		Kind: string(workflow.TriggerKindPush),
		Push: nil,
		Repo: &tangled.Pipeline_TriggerRepo{
			Knot:    "example.com",
			Did:     "did:plc:user123",
			Repo:    sp("my-repo"),
			RepoDid: sp("did:plc:boltless"),
		},
	}
	id := PipelineId{
		Knot: "example.com",
		Rkey: "123123",
	}
	env := PipelineEnvVars(tr, id, false)

	// Should still have repo variables
	if env["TANGLED_REPO_KNOT"] != "example.com" {
		t.Errorf("Expected TANGLED_REPO_KNOT='example.com', got '%s'", env["TANGLED_REPO_KNOT"])
	}

	// Should not have ref/sha variables
	if _, ok := env["TANGLED_REF"]; ok {
		t.Error("Should not have TANGLED_REF when push data is nil")
	}
}
