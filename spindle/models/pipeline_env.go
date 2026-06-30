package models

import (
	"strings"

	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/api/tangled"
	"tangled.org/core/workflow"
)

// PipelineEnvVars builds the standard CI environment variables for a pipeline
func PipelineEnvVars(tr *tangled.Pipeline_TriggerMetadata, pipelineId PipelineId) map[string]string {
	if tr == nil {
		return nil
	}

	env := make(map[string]string)

	// standard CI env vars
	env["CI"] = "true"

	env["TANGLED_PIPELINE_ID"] = pipelineId.AtUri().String()
	env["TANGLED_PIPELINE_KIND"] = tr.Kind

	// repo info
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
		env["TANGLED_REPO_URL"] = BuildRepoURL(tr.Repo)
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
			// for PRs, ref is the source branch
			env["TANGLED_REF"] = "refs/heads/" + tr.PullRequest.SourceBranch
			env["TANGLED_REF_NAME"] = tr.PullRequest.SourceBranch
			env["TANGLED_REF_TYPE"] = "branch"
			env["TANGLED_SHA"] = tr.PullRequest.SourceSha
			env["TANGLED_COMMIT_SHA"] = tr.PullRequest.SourceSha

			// PR-specific env vars
			env["TANGLED_PR_SOURCE_BRANCH"] = tr.PullRequest.SourceBranch
			env["TANGLED_PR_TARGET_BRANCH"] = tr.PullRequest.TargetBranch
			env["TANGLED_PR_SOURCE_SHA"] = tr.PullRequest.SourceSha
			env["TANGLED_PR_ACTION"] = tr.PullRequest.Action
		}

	case workflow.TriggerKindManual:
		if tr.Manual != nil {
			env["TANGLED_SHA"] = tr.Manual.Sha
			env["TANGLED_COMMIT_SHA"] = tr.Manual.Sha
			if tr.Manual.Ref != nil && *tr.Manual.Ref != "" {
				refName := plumbing.ReferenceName(*tr.Manual.Ref)
				refType := "branch"
				if refName.IsTag() {
					refType = "tag"
				}
				env["TANGLED_REF"] = *tr.Manual.Ref
				env["TANGLED_REF_NAME"] = refName.Short()
				env["TANGLED_REF_TYPE"] = refType
			}
			// include manual inputs if present
			for _, pair := range tr.Manual.Inputs {
				env["TANGLED_INPUT_"+strings.ToUpper(pair.Key)] = pair.Value
			}
		}
	}

	return env
}
