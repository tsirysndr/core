package models

import (
	"fmt"
	"strings"

	"tangled.org/core/api/tangled"
	"tangled.org/core/hostutil"
	"tangled.org/core/workflow"
)

type CloneStep struct {
	name     string
	kind     StepKind
	commands []string
}

func (s CloneStep) Name() string {
	return s.name
}

func (s CloneStep) Commands() []string {
	return s.commands
}

func (s CloneStep) Command() string {
	return strings.Join(s.commands, "\n")
}

func (s CloneStep) Kind() StepKind {
	return s.kind
}

// BuildCloneStep generates git clone commands.
// The caller must ensure the current working directory is set to the desired
// workspace directory before executing these commands.
//
// The generated commands are:
// - git init
// - git remote add origin <url>
// - git fetch --depth=<d> --recurse-submodules=<yes|no> <sha>
// - git checkout FETCH_HEAD
//
// Supports all trigger types (push, PR, manual) and clone options.
func BuildCloneStep(twf tangled.Pipeline_Workflow, tr tangled.Pipeline_TriggerMetadata, dev bool) CloneStep {
	if twf.Clone != nil && twf.Clone.Skip {
		return CloneStep{}
	}

	commitSHA, err := extractCommitSHA(tr)
	if err != nil {
		return CloneStep{
			kind:     StepKindSystem,
			name:     "Clone repository into workspace (error)",
			commands: []string{fmt.Sprintf("echo 'Failed to get clone info: %s' && exit 1", err.Error())},
		}
	}

	repoURL := BuildRepoURL(tr.Repo)

	var cloneOpts tangled.Pipeline_CloneOpts
	if twf.Clone != nil {
		cloneOpts = *twf.Clone
	}
	fetchArgs := buildFetchArgs(cloneOpts, commitSHA)

	// In dev mode we point at Caddy via host-gateway with a self-signed cert,
	// so skip the TLS check for the fetch call.
	fetchCmd := "git fetch"
	if dev {
		fetchCmd = "git -c http.sslVerify=false fetch"
	}

	return CloneStep{
		kind: StepKindSystem,
		name: "Clone repository into workspace",
		commands: []string{
			"git init",
			fmt.Sprintf("git remote add origin %s", repoURL),
			fmt.Sprintf("%s %s", fetchCmd, strings.Join(fetchArgs, " ")),
			"git checkout FETCH_HEAD",
		},
	}
}

// extractCommitSHA extracts the commit SHA from trigger metadata based on trigger type
func extractCommitSHA(tr tangled.Pipeline_TriggerMetadata) (string, error) {
	switch workflow.TriggerKind(tr.Kind) {
	case workflow.TriggerKindPush:
		if tr.Push == nil {
			return "", fmt.Errorf("push trigger metadata is nil")
		}
		return tr.Push.NewSha, nil

	case workflow.TriggerKindPullRequest:
		if tr.PullRequest == nil {
			return "", fmt.Errorf("pull request trigger metadata is nil")
		}
		return tr.PullRequest.SourceSha, nil

	case workflow.TriggerKindManual:
		if tr.Manual == nil {
			return "", fmt.Errorf("manual trigger metadata is nil")
		}
		return tr.Manual.Sha, nil

	default:
		return "", fmt.Errorf("unknown trigger kind: %s", tr.Kind)
	}
}

// BuildRepoURL constructs the repository URL from repo metadata.
func BuildRepoURL(repo *tangled.Pipeline_TriggerRepo) string {
	if repo == nil || repo.RepoDid == nil {
		return ""
	}
	host, noSSL, _ := hostutil.ParseHostname(repo.Knot)
	scheme := "https"
	if noSSL {
		scheme = "http"
	}
	return fmt.Sprintf("%s://%s/%s", scheme, host, *repo.RepoDid)
}

// buildFetchArgs constructs the arguments for git fetch based on clone options
func buildFetchArgs(clone tangled.Pipeline_CloneOpts, sha string) []string {
	args := []string{}

	// Set fetch depth (default to 1 for shallow clone)
	depth := clone.Depth
	if depth == 0 {
		depth = 1
	}
	args = append(args, fmt.Sprintf("--depth=%d", depth))

	// Add submodules if requested
	if clone.Submodules {
		args = append(args, "--recurse-submodules=yes")
	}

	// Add tags if requested
	if clone.Tags {
		args = append(args, "--tags")
	}

	// Add remote and SHA
	args = append(args, "origin")
	if sha != "" {
		args = append(args, sha)
	}

	return args
}
