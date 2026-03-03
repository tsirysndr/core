package models

import (
	"strings"

	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/workflow"
)

// PipelineEnvVars extracts environment variables from pipeline trigger metadata.
// These are framework-provided variables that are injected into workflow steps.
func PipelineEnvVars(tr *tangled.Pipeline_TriggerMetadata, pipelineId PipelineId, devMode bool) map[string]string {
	if tr == nil {
		return nil
	}

	env := make(map[string]string)

	// Standard CI environment variable
	env["CI"] = "true"

	env["TANGLED_PIPELINE_ID"] = pipelineId.AtUri().String()

	// Repo info
	if tr.Repo != nil {
		env["TANGLED_REPO_KNOT"] = tr.Repo.Knot
		env["TANGLED_REPO_DID"] = tr.Repo.Did
		if tr.Repo.Repo != nil {
			env["TANGLED_REPO_NAME"] = *tr.Repo.Repo
		}
		if tr.Repo.RepoDid != nil {
			env["TANGLED_REPO_REPO_DID"] = *tr.Repo.RepoDid
		}
		env["TANGLED_REPO_DEFAULT_BRANCH"] = tr.Repo.DefaultBranch
		env["TANGLED_REPO_URL"] = BuildRepoURL(tr.Repo, devMode)
	}

	switch workflow.TriggerKind(tr.Kind) {
	case workflow.TriggerKindPush:
		if tr.Push != nil {
			refName := plumbing.ReferenceName(tr.Push.Ref)
			refType := "branch"
			if refName.IsTag() {
				refType = "tag"
			}

			env["TANGLED_REF"] = tr.Push.Ref
			env["TANGLED_REF_NAME"] = refName.Short()
			env["TANGLED_REF_TYPE"] = refType
			env["TANGLED_SHA"] = tr.Push.NewSha
			env["TANGLED_COMMIT_SHA"] = tr.Push.NewSha
		}

	case workflow.TriggerKindPullRequest:
		if tr.PullRequest != nil {
			// For PRs, the "ref" is the source branch
			env["TANGLED_REF"] = "refs/heads/" + tr.PullRequest.SourceBranch
			env["TANGLED_REF_NAME"] = tr.PullRequest.SourceBranch
			env["TANGLED_REF_TYPE"] = "branch"
			env["TANGLED_SHA"] = tr.PullRequest.SourceSha
			env["TANGLED_COMMIT_SHA"] = tr.PullRequest.SourceSha

			// PR-specific variables
			env["TANGLED_PR_SOURCE_BRANCH"] = tr.PullRequest.SourceBranch
			env["TANGLED_PR_TARGET_BRANCH"] = tr.PullRequest.TargetBranch
			env["TANGLED_PR_SOURCE_SHA"] = tr.PullRequest.SourceSha
			env["TANGLED_PR_ACTION"] = tr.PullRequest.Action
		}

	case workflow.TriggerKindManual:
		// Manual triggers may not have ref/sha info
		// Include any manual inputs if present
		if tr.Manual != nil {
			for _, pair := range tr.Manual.Inputs {
				env["TANGLED_INPUT_"+strings.ToUpper(pair.Key)] = pair.Value
			}
		}
	}

	return env
}
