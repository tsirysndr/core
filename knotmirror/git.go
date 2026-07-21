package knotmirror

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/go-git/go-git/v5"
	gitconfig "github.com/go-git/go-git/v5/config"
	"github.com/go-git/go-git/v5/plumbing/transport"
	"tangled.org/core/knotmirror/models"
)

type branch struct {
	Name    string `json:"name"`
	Version string `json:"version"`
}

type GitMirrorManager interface {
	Exist(repo *models.Repo) (bool, error)
	// Clone clones the repository as a mirror
	Clone(ctx context.Context, repo *models.Repo) error
	// Fetch fetches the repository
	Fetch(ctx context.Context, repo *models.Repo) error
	// Sync mirrors the repository. It will clone the repository if repository doesn't exist.
	Sync(ctx context.Context, repo *models.Repo) error
	DefaultBranch(ctx context.Context, repo *models.Repo) (branch, error)
	Delete(repo *models.Repo) error
}

type CliGitMirrorManager struct {
	repoBasePath string
	knotUseSSL   bool
}

func NewCliGitMirrorManager(repoBasePath string, knotUseSSL bool) *CliGitMirrorManager {
	return &CliGitMirrorManager{
		repoBasePath,
		knotUseSSL,
	}
}

var _ GitMirrorManager = new(CliGitMirrorManager)

func (c *CliGitMirrorManager) makeRepoPath(repo *models.Repo) string {
	return filepath.Join(c.repoBasePath, repo.RepoDid.String())
}

func (c *CliGitMirrorManager) Exist(repo *models.Repo) (bool, error) {
	return isDir(c.makeRepoPath(repo))
}

func (c *CliGitMirrorManager) Clone(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}
	return c.clone(ctx, path, url)
}

func (c *CliGitMirrorManager) clone(ctx context.Context, path, url string) error {
	cmd := exec.CommandContext(ctx, "git", "clone", "--mirror", url, path)
	if out, err := cmd.CombinedOutput(); err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		msg := string(out)
		if classification := classifyCliError(msg); classification != nil {
			return classification
		}
		return fmt.Errorf("running 'git clone --mirror %s': %w\n%s", url, err, msg)
	}
	writeCommitGraph(ctx, path, 30*time.Second)
	return nil
}

func (c *CliGitMirrorManager) Fetch(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}
	return c.fetch(ctx, path, url)
}

func (c *CliGitMirrorManager) fetch(ctx context.Context, path, url string) error {
	cmd := exec.CommandContext(ctx, "git", "-C", path, "fetch", "--prune", url, "+refs/*:refs/*")
	if out, err := cmd.CombinedOutput(); err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return fmt.Errorf("running 'git fetch': %w\n%s", err, string(out))
	}

	// TODO(boltless): make this dedicated event instead
	lsRemoteCmd := exec.CommandContext(ctx, "git", "ls-remote", "--symref", url, "HEAD")
	out, err := lsRemoteCmd.CombinedOutput()
	if err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return fmt.Errorf("running 'git ls-remote --symref': %w\n%s", err, string(out))
	}

	var headRef string
	for line := range strings.SplitSeq(string(out), "\n") {
		if !strings.HasPrefix(line, "ref: ") {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) >= 2 {
			headRef = fields[1]
			break
		}
	}
	if headRef != "" {
		symrefCmd := exec.CommandContext(ctx, "git", "-C", path, "symbolic-ref", "HEAD", headRef)
		if out, err := symrefCmd.CombinedOutput(); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return fmt.Errorf("running 'git symbolic-ref HEAD %s': %w\n%s", headRef, err, string(out))
		}
	}

	writeCommitGraph(ctx, path, 3*time.Second)
	return nil
}

func writeCommitGraph(ctx context.Context, path string, timeout time.Duration) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	if err := exec.CommandContext(ctx, "git", "-C", path, "commit-graph", "write", "--reachable", "--split").Run(); err != nil {
		log.Println("failed to run commit-graph", err)
	}
}

func (c *CliGitMirrorManager) Sync(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}

	exist, err := isDir(path)
	if err != nil {
		return fmt.Errorf("checking repo path: %w", err)
	}
	if !exist {
		if err := c.clone(ctx, path, url); err != nil {
			return fmt.Errorf("cloning repo: %w", err)
		}
	} else {
		if err := c.fetch(ctx, path, url); err != nil {
			return fmt.Errorf("fetching repo: %w", err)
		}
	}
	return nil
}

func (c *CliGitMirrorManager) DefaultBranch(ctx context.Context, repo *models.Repo) (branch, error) {
	path := c.makeRepoPath(repo)

	nameCmd := exec.CommandContext(ctx, "git", "-C", path, "symbolic-ref", "--short", "HEAD")
	nameOut, err := nameCmd.Output()
	if err != nil {
		return branch{}, err
	}

	// --verify --quiet exits 1 with no output on an empty repo (unborn HEAD).
	revCmd := exec.CommandContext(ctx, "git", "-C", path, "rev-parse", "--verify", "--quiet", "HEAD")
	revOut, err := revCmd.Output()
	if err != nil {
		return branch{}, err
	}

	version := strings.TrimSpace(string(revOut))
	if version == "" {
		return branch{}, errors.New("git: no commits")
	}

	return branch{
		Name:    strings.TrimSpace(string(nameOut)),
		Version: version,
	}, nil
}

func (c *CliGitMirrorManager) Delete(repo *models.Repo) error {
	return os.RemoveAll(c.makeRepoPath(repo))
}

var (
	ErrDNSFailure   = errors.New("git: knot: dns failure (could not resolve host)")
	ErrCertExpired  = errors.New("git: knot: certificate has expired")
	ErrCertMismatch = errors.New("git: knot: certificate hostname mismatch")
	ErrTLSHandshake = errors.New("git: knot: tls handshake failure")
	ErrHTTPStatus   = errors.New("git: knot: request url returned error")
	ErrUnreachable  = errors.New("git: knot: could not connect to server")
	ErrRepoNotFound = errors.New("git: repo: repository not found")
)

var (
	reDNSFailure   = regexp.MustCompile(`Could not resolve host:`)
	reCertExpired  = regexp.MustCompile(`SSL certificate OpenSSL verify result: certificate has expired`)
	reCertMismatch = regexp.MustCompile(`SSL: no alternative certificate subject name matches target hostname`)
	reTLSHandshake = regexp.MustCompile(`TLS connect error: (.*)`)
	reHTTPStatus   = regexp.MustCompile(`The requested URL returned error: (\d\d\d)`)
	reUnreachable  = regexp.MustCompile(`Could not connect to server`)
	reRepoNotFound = regexp.MustCompile(`repository '.*?' not found`)
)

// classifyCliError classifies git cli error message. It will return nil for unknown error messages
func classifyCliError(stderr string) error {
	msg := strings.TrimSpace(stderr)
	if m := reTLSHandshake.FindStringSubmatch(msg); len(m) > 1 {
		return fmt.Errorf("%w: %s", ErrTLSHandshake, m[1])
	}
	if m := reHTTPStatus.FindStringSubmatch(msg); len(m) > 1 {
		return fmt.Errorf("%w: %s", ErrHTTPStatus, m[1])
	}
	switch {
	case reDNSFailure.MatchString(msg):
		return ErrDNSFailure
	case reCertExpired.MatchString(msg):
		return ErrCertExpired
	case reCertMismatch.MatchString(msg):
		return ErrCertMismatch
	case reUnreachable.MatchString(msg):
		return ErrUnreachable
	case reRepoNotFound.MatchString(msg):
		return ErrRepoNotFound
	}
	return nil
}

type GoGitMirrorManager struct {
	repoBasePath string
	knotUseSSL   bool
}

func NewGoGitMirrorClient(repoBasePath string, knotUseSSL bool) *GoGitMirrorManager {
	return &GoGitMirrorManager{
		repoBasePath,
		knotUseSSL,
	}
}

var _ GitMirrorManager = new(GoGitMirrorManager)

func (c *GoGitMirrorManager) makeRepoPath(repo *models.Repo) string {
	return filepath.Join(c.repoBasePath, repo.RepoDid.String())
}

func (c *GoGitMirrorManager) Exist(repo *models.Repo) (bool, error) {
	return isDir(c.makeRepoPath(repo))
}

func (c *GoGitMirrorManager) Clone(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}
	return c.clone(ctx, path, url)
}

func (c *GoGitMirrorManager) clone(ctx context.Context, path, url string) error {
	_, err := git.PlainCloneContext(ctx, path, true, &git.CloneOptions{
		URL:    url,
		Mirror: true,
	})
	if err != nil && !errors.Is(err, transport.ErrEmptyRemoteRepository) {
		return fmt.Errorf("cloning repo: %w", err)
	}
	return nil
}

func (c *GoGitMirrorManager) Fetch(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}

	return c.fetch(ctx, path, url)
}

func (c *GoGitMirrorManager) fetch(ctx context.Context, path, url string) error {
	gr, err := git.PlainOpen(path)
	if err != nil {
		return fmt.Errorf("opening local repo: %w", err)
	}
	if err := gr.FetchContext(ctx, &git.FetchOptions{
		RemoteURL: url,
		RefSpecs:  []gitconfig.RefSpec{gitconfig.RefSpec("+refs/*:refs/*")},
		Force:     true,
		Prune:     true,
	}); err != nil {
		return fmt.Errorf("fetching reppo: %w", err)
	}
	return nil
}

func (c *GoGitMirrorManager) Sync(ctx context.Context, repo *models.Repo) error {
	path := c.makeRepoPath(repo)
	url, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), c.knotUseSSL)
	if err != nil {
		return fmt.Errorf("constructing repo remote url: %w", err)
	}

	exist, err := isDir(path)
	if err != nil {
		return fmt.Errorf("checking repo path: %w", err)
	}
	if !exist {
		if err := c.clone(ctx, path, url); err != nil {
			return fmt.Errorf("cloning repo: %w", err)
		}
	} else {
		if err := c.fetch(ctx, path, url); err != nil {
			return fmt.Errorf("fetching repo: %w", err)
		}
	}
	return nil
}

func (c *GoGitMirrorManager) DefaultBranch(ctx context.Context, repo *models.Repo) (branch, error) {
	gr, err := git.PlainOpen(c.makeRepoPath(repo))
	if err != nil {
		return branch{}, fmt.Errorf("opening local repo: %w", err)
	}
	ref, err := gr.Head()
	if err != nil {
		return branch{}, fmt.Errorf("resolving HEAD: %w", err)
	}
	return branch{
		Name:    ref.Name().Short(),
		Version: ref.Hash().String(),
	}, nil
}

func (c *GoGitMirrorManager) Delete(repo *models.Repo) error {
	return os.RemoveAll(c.makeRepoPath(repo))
}

func makeRepoRemoteUrl(knot, repoIdentifier string, knotUseSSL bool) (string, error) {
	if !strings.Contains(knot, "://") {
		if knotUseSSL {
			knot = "https://" + knot
		} else {
			knot = "http://" + knot
		}
	}

	u, err := url.Parse(knot)
	if err != nil {
		return "", err
	}

	if u.Scheme != "http" && u.Scheme != "https" {
		return "", fmt.Errorf("unsupported scheme: %s", u.Scheme)
	}

	u = u.JoinPath(repoIdentifier)
	return u.String(), nil
}

func isDir(path string) (bool, error) {
	info, err := os.Stat(path)
	if err == nil && info.IsDir() {
		return true, nil
	}
	if os.IsNotExist(err) {
		return false, nil
	}
	return false, err
}
