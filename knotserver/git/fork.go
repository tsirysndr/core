package git

import (
	"errors"
	"fmt"
	"log/slog"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/config"
	knotconfig "tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/sandbox"
)

func Fork(repoPath, source string, cfg *knotconfig.Config) error {
	return ForkWithSandbox(repoPath, source, cfg, nil)
}

// ForkWithSandbox clones source into repoPath, optionally wrapping the
// post-clone configure step in sb. The initial clone itself is not sandboxed
// because the target directory doesn't exist yet when the ruleset is applied.
func ForkWithSandbox(repoPath, source string, cfg *knotconfig.Config, sb sandbox.Backend) error {
	if !(source == "" || source[0] != '-') {
		return fmt.Errorf("invalid source: %q", source)
	}
	u, err := url.Parse(source)
	if err != nil {
		return fmt.Errorf("failed to parse source URL: %w", err)
	}
	if u.Scheme != "https" && u.Scheme != "http" {
		return fmt.Errorf("invalid scheme: %q", u.Scheme)
	}
	if u.Host == "" {
		return fmt.Errorf("missing host: %q", source)
	}

	if o := optimizeClone(u, cfg); o != nil {
		u = o
	}

	cloneCmd := exec.Command(
		"git",
		"-c", "protocol.ext.allow=never",
		"clone", "--bare", u.String(), repoPath,
	)
	cloneCmd.Env = append(cloneCmd.Env, "GIT_PROTOCOL_FROM_USER=0")
	cloneCmd.Env = append(cloneCmd.Env, "GIT_TERMINAL_PROMPT=0")
	if err := cloneCmd.Run(); err != nil {
		return fmt.Errorf("failed to bare clone repository: %w", err)
	}

	// ensure repoPath exists before attempting to sandbox the configure step.
	if _, statErr := os.Stat(repoPath); statErr != nil {
		return fmt.Errorf("clone did not create %s: %w", repoPath, statErr)
	}

	configureCmd := exec.Command("git", "-C", repoPath, "config", "receive.hideRefs", "refs/hidden")
	if sb != nil {
		configureCmd, err = sb.Wrap(repoPath, configureCmd)
		if err != nil {
			return fmt.Errorf("sandbox wrap for git config: %w", err)
		}
	} else {
		configureCmd.Dir = repoPath
	}
	if err := configureCmd.Run(); err != nil {
		return fmt.Errorf("failed to configure hidden refs: %w", err)
	}

	return nil
}

func optimizeClone(u *url.URL, cfg *knotconfig.Config) *url.URL {
	// only optimize if it's the same host
	if u.Host != cfg.Server.Hostname {
		return nil
	}

	local := filepath.Join(cfg.Repo.ScanPath, u.Path)

	// sanity check: is there a git repo there?
	if _, err := PlainOpen(local); err != nil {
		return nil
	}

	// create optimized file:// URL
	optimized := &url.URL{
		Scheme: "file",
		Path:   local,
	}

	slog.Debug("performing local clone", "url", optimized.String())
	return optimized
}

func (g *GitRepo) Sync() error {
	branch := g.h.String()

	fetchOpts := &git.FetchOptions{
		RefSpecs: []config.RefSpec{
			config.RefSpec("+" + branch + ":" + branch), // +refs/heads/master:refs/heads/master
		},
	}

	err := g.r.Fetch(fetchOpts)
	if errors.Is(git.NoErrAlreadyUpToDate, err) {
		return nil
	} else if err != nil {
		return fmt.Errorf("failed to fetch origin branch: %s: %w", branch, err)
	}
	return nil
}

// TrackHiddenRemoteRef tracks a hidden remote in the repository. For example,
// if the feature branch on the fork (forkRef) is feature-1, and the remoteRef,
// i.e. the branch we want to merge into, is main, this will result in a refspec:
//
//	+refs/heads/main:refs/hidden/feature-1/main
func (g *GitRepo) TrackHiddenRemoteRef(forkRef, remoteRef string) error {
	fetchOpts := &git.FetchOptions{
		RefSpecs: []config.RefSpec{
			config.RefSpec(fmt.Sprintf("+refs/heads/%s:refs/hidden/%s/%s", remoteRef, forkRef, remoteRef)),
		},
		RemoteName: "origin",
	}

	err := g.r.Fetch(fetchOpts)
	if errors.Is(git.NoErrAlreadyUpToDate, err) {
		return nil
	} else if err != nil {
		return fmt.Errorf("failed to fetch hidden remote: %s: %w", forkRef, err)
	}
	return nil
}
