package git

import (
	"bufio"
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"iter"
	"os/exec"
	"path"
	"strconv"
	"strings"
	"time"

	"github.com/dgraph-io/ristretto"
	"github.com/go-git/go-git/v5/plumbing"
	"tangled.org/core/sets"
	"tangled.org/core/types"
)

var (
	commitCache *ristretto.Cache
)

func init() {
	cache, _ := ristretto.NewCache(&ristretto.Config{
		NumCounters:            1e7,
		MaxCost:                1 << 30,
		BufferItems:            64,
		TtlTickerDurationInSec: 120,
	})
	commitCache = cache
}

// processReader wraps a reader and ensures the associated process is cleaned up
type processReader struct {
	io.Reader
	cmd    *exec.Cmd
	stdout io.ReadCloser
}

func (pr *processReader) Close() error {
	if err := pr.stdout.Close(); err != nil {
		return err
	}
	return pr.cmd.Wait()
}

func (g *GitRepo) streamingGitLog(ctx context.Context, extraArgs ...string) (io.ReadCloser, error) {
	args := []string{}
	args = append(args, "log")
	args = append(args, g.h.String())
	args = append(args, extraArgs...)

	cmd := exec.CommandContext(ctx, "git", args...)
	cmd.Dir = g.path

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}

	if err := cmd.Start(); err != nil {
		return nil, err
	}

	return &processReader{
		Reader: stdout,
		cmd:    cmd,
		stdout: stdout,
	}, nil
}

type commit struct {
	hash    plumbing.Hash
	when    time.Time
	files   sets.Set[string]
	message string
}

func newCommit() commit {
	return commit{
		files: sets.New[string](),
	}
}

type lastCommitDir struct {
	dir     string
	entries []string
}

func (l lastCommitDir) children() iter.Seq[string] {
	return func(yield func(string) bool) {
		for _, child := range l.entries {
			if !yield(path.Join(l.dir, child)) {
				return
			}
		}
	}
}

func cacheKey(g *GitRepo, path string) string {
	sep := byte(':')
	hash := sha256.Sum256(fmt.Append([]byte{}, g.path, sep, g.h.String(), sep, path))
	return fmt.Sprintf("%x", hash)
}

func (g *GitRepo) lastCommitDirIn(ctx context.Context, parent lastCommitDir, timeout time.Duration) (map[string]commit, error) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	return g.lastCommitDir(ctx, parent)
}

func (g *GitRepo) lastCommitDir(ctx context.Context, parent lastCommitDir) (map[string]commit, error) {
	filesToDo := sets.Collect(parent.children())
	filesDone := make(map[string]commit)

	for p := range filesToDo.All() {
		cacheKey := cacheKey(g, p)
		if cached, ok := commitCache.Get(cacheKey); ok {
			filesDone[p] = cached.(commit)
			filesToDo.Remove(p)
		} else {
			filesToDo.Insert(p)
		}
	}

	if filesToDo.IsEmpty() {
		return filesDone, nil
	}

	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	pathSpec := "."
	if parent.dir != "" {
		pathSpec = parent.dir
	}
	if filesToDo.Len() == 1 {
		// this is an optimization for the scenario where we want to calculate
		// the last commit for just one path, we can directly set the pathspec to that path
		for s := range filesToDo.All() {
			pathSpec = s
		}
	}

	output, err := g.streamingGitLog(ctx, "--pretty=format:%H,%cd,%s", "--date=unix", "--name-only", "--", pathSpec)
	if err != nil {
		return nil, err
	}
	defer output.Close() // Ensure the git process is properly cleaned up

	reader := bufio.NewReader(output)
	current := newCommit()
	for {
		line, err := reader.ReadString('\n')
		if err != nil && err != io.EOF {
			return nil, err
		}
		line = strings.TrimSpace(line)

		if line == "" {
			if !current.hash.IsZero() {
				// we have a fully parsed commit
				for f := range current.files.All() {
					if filesToDo.Contains(f) {
						filesDone[f] = current
						filesToDo.Remove(f)
						commitCache.Set(cacheKey(g, f), current, 0)
					}
				}

				if filesToDo.IsEmpty() {
					break
				}
				current = newCommit()
			}
		} else if current.hash.IsZero() {
			parts := strings.SplitN(line, ",", 3)
			if len(parts) == 3 {
				current.hash = plumbing.NewHash(parts[0])
				epochTime, _ := strconv.ParseInt(parts[1], 10, 64)
				current.when = time.Unix(epochTime, 0)
				current.message = parts[2]
			}
		} else {
			// all ancestors along this path should also be included
			file := path.Clean(line)
			current.files.Insert(file)
			for _, a := range ancestors(file) {
				current.files.Insert(a)
			}
		}

		if err == io.EOF {
			break
		}
	}

	return filesDone, nil
}

// LastCommitFile returns the last commit information for a specific file path
func (g *GitRepo) LastCommitFile(ctx context.Context, filePath string) (*types.LastCommitInfo, error) {
	parent, child := path.Split(filePath)
	parent = path.Clean(parent)
	if parent == "." {
		parent = ""
	}

	lastCommitDir := lastCommitDir{
		dir:     parent,
		entries: []string{child},
	}

	times, err := g.lastCommitDirIn(ctx, lastCommitDir, 2*time.Second)
	if err != nil {
		return nil, fmt.Errorf("calculate commit time: %w", err)
	}

	// extract the only element of the map, the commit info of the current path
	var commitInfo *commit
	for _, c := range times {
		commitInfo = &c
	}

	if commitInfo == nil {
		return nil, fmt.Errorf("no commit found for path: %s", filePath)
	}

	return &types.LastCommitInfo{
		Hash:    commitInfo.hash,
		Message: commitInfo.message,
		When:    commitInfo.when,
	}, nil
}

func ancestors(p string) []string {
	var ancestors []string

	for {
		p = path.Dir(p)
		if p == "." || p == "/" {
			break
		}
		ancestors = append(ancestors, p)
	}
	return ancestors
}
