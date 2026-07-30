// similar to zoekt-dynamic-indexserver, but targetting Tangled repos
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/carlmjohnson/versioninfo"
	"github.com/samber/lo"
	"github.com/sourcegraph/zoekt"
	"github.com/sourcegraph/zoekt/gitindex"
	"github.com/sourcegraph/zoekt/index"
	"github.com/urfave/cli/v3"
	"tangled.org/core/repoident"
)

func loggedRun(cmd *exec.Cmd) error {
	outBuf := &bytes.Buffer{}
	errBuf := &bytes.Buffer{}
	cmd.Stdout = outBuf
	cmd.Stderr = errBuf

	log.Printf("run %v", cmd.Args)
	if err := cmd.Run(); err != nil {
		log.Printf("command %s failed: %v\nOUT: %s\nERR: %s",
			cmd.Args, err, outBuf.String(), errBuf.String())
		return fmt.Errorf("command %s failed: %v", cmd.Args, err)
	}

	return nil
}

// This function is declared as var so that we can stub it in test
var executeCmd = func(ctx context.Context, name string, arg ...string) error {
	cmd := exec.CommandContext(ctx, name, arg...)
	cmd.Stdin = &bytes.Buffer{}
	err := loggedRun(cmd)

	return err
}

func main() {
	if err := run(os.Args); err != nil {
		log.Fatal(err)
	}
}

func run(args []string) error {
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	app := cli.Command{
		Name:    "zoekt-tngl-indexserver",
		Usage:   "tangled zoekt index server",
		Version: versioninfo.Short(),
	}
	app.Flags = []cli.Flag{
		&cli.StringFlag{
			Name:     "index-dir",
			Usage:    "directory holding index shards.",
			Required: true,
			Sources:  cli.EnvVars("TANGLED_ZOEKT_INDEX_DIR"),
		},
		&cli.StringFlag{
			Name:    "appview-url",
			Usage:   "appview url. used when debugging",
			Value:   "https://tangled.org",
			Sources: cli.EnvVars("TANGLED_ZOEKT_APPVIEW_URL"),
		},
	}
	app.Commands = []*cli.Command{
		{
			Name:   "serve",
			Usage:  "run the index server daemon",
			Action: runIndexServer,
			Flags: []cli.Flag{
				&cli.StringFlag{
					Name:    "plc-url",
					Usage:   "atproto PLC directory.",
					Value:   "https://plc.directory",
					Sources: cli.EnvVars("TANGLED_ZOEKT_PLC_URL", "TANGLED_PLC_URL", "ATP_PLC_HOST"),
				},
				&cli.DurationFlag{
					Name:    "index-timeout",
					Usage:   "kill index job after this much time.",
					Value:   time.Hour,
					Sources: cli.EnvVars("TANGLED_ZOEKT_INDEX_TIMEOUT"),
				},
				&cli.IntFlag{
					Name:    "index-concurrency",
					Usage:   "number of repositories to index concurrently.",
					Value:   4,
					Sources: cli.EnvVars("TANGLED_ZOEKT_INDEX_CONCURRENCY"),
				},
				&cli.IntFlag{
					Name:    "index-queue-size",
					Usage:   "max index queue size",
					Value:   100,
					Sources: cli.EnvVars("TANGLED_ZOEKT_INDEX_QUEUE_SIZE"),
				},
				&cli.StringFlag{
					Name:    "listen",
					Usage:   "listen on this address",
					Value:   ":6060",
					Sources: cli.EnvVars("TANGLED_ZOEKT_SERVER_LISTEN"),
				},
				&cli.BoolFlag{
					Name:    "allow-http",
					Usage:   "accept repo DIDs whose knot service endpoint is plaintext http.",
					Sources: cli.EnvVars("TANGLED_ZOEKT_ALLOW_HTTP"),
				},
			},
		},
		{
			Name:   "index",
			Usage:  "manually index a git repository",
			Action: runIndex,
			Arguments: []cli.Argument{
				&cli.StringArg{
					Name:      "git-dir",
					UsageText: "path to fetched git repository.",
				},
				&cli.StringArg{
					Name:      "repo",
					UsageText: "json-encoded repository information",
				},
			},
		},
	}

	return app.Run(ctx, args)
}

type Config struct {
	// IndexDir is the index directory to use.
	IndexDir string

	IndexTimeout time.Duration

	// IndexConcurrency is the number of repositories we index at once.
	IndexConcurrency int
	IndexQueueSize   int

	PlcUrl     string
	AppviewUrl string
	Listen     string

	KnotScheme repoident.SchemePolicy
}

func createMissingDirectories(cfg *Config) {
	for _, s := range []string{cfg.IndexDir} {
		if err := os.MkdirAll(s, 0o755); err != nil {
			log.Fatalf("MkdirAll %s: %v", s, err)
		}
	}
}

type Repo struct {
	Did      repoident.RepoDid
	Owner    repoident.OwnerDid
	Slug     syntax.RecordKey
	Knot     repoident.KnotURL
	Branches []zoekt.RepositoryBranch
}

func (r *Repo) CloneURL() string {
	return r.Knot.JoinPath(r.Did.String())
}

func runIndexServer(ctx context.Context, cmd *cli.Command) error {
	cfg := &Config{
		IndexDir:         cmd.String("index-dir"),
		IndexTimeout:     cmd.Duration("index-timeout"),
		IndexConcurrency: cmd.Int("index-concurrency"),
		IndexQueueSize:   cmd.Int("index-queue-size"),
		PlcUrl:           cmd.String("plc-url"),
		AppviewUrl:       cmd.String("appview-url"),
		Listen:           cmd.String("listen"),
		KnotScheme:       repoident.SchemeFor(cmd.Bool("allow-http")),
	}
	createMissingDirectories(cfg)

	server := NewIndexServer(cfg)
	go server.Run(ctx)

	<-ctx.Done()
	return ctx.Err()
}

// sub-process to index a fetched repository
func runIndex(ctx context.Context, cmd *cli.Command) error {
	var (
		indexDir   = cmd.String("index-dir")
		appviewUrl = cmd.String("appview-url")
	)

	gitDir := cmd.StringArg("git-dir")
	if gitDir == "" {
		return errors.New("git-dir is required.")
	}

	repoRaw := cmd.StringArg("repo")
	if repoRaw == "" {
		return errors.New("repo is required.")
	}

	var repo Repo
	if err := json.Unmarshal([]byte(repoRaw), &repo); err != nil {
		return fmt.Errorf("invalid repo: %w", err)
	}
	if repo.Did == "" || repo.Owner == "" || repo.Knot.IsZero() {
		return fmt.Errorf("repo is missing did, owner, or knot: %q", repoRaw)
	}

	branches := lo.Map(repo.Branches, func(b zoekt.RepositoryBranch, _ int) string { return b.Name })

	buildOpts := index.Options{}
	buildOpts.SetDefaults()

	buildOpts.IndexDir = indexDir

	buildOpts.ShardPrefixOverride = repo.Did.String()

	// Tangled templates
	webUrl := fmt.Sprintf("%s/%s", appviewUrl, repo.Did)
	buildOpts.RepositoryDescription.CommitURLTemplate = fmt.Sprintf("%s/commit/{{.Version}}", webUrl)
	buildOpts.RepositoryDescription.FileURLTemplate = fmt.Sprintf("%s/blob/{{.Version}}/{{.Path}}", webUrl)
	buildOpts.RepositoryDescription.LineFragmentTemplate = "#L{{.LineNumber}}"

	buildOpts.RepositoryDescription.Name = repo.Slug.String()
	buildOpts.RepositoryDescription.URL = webUrl
	buildOpts.RepositoryDescription.Metadata = map[string]string{
		"foo":   "bar", // for testing
		"did":   repo.Did.String(),
		"owner": repo.Owner.String(),
		"knot":  repo.Knot.String(),
	}
	// buildOpts.RepositoryDescription.Source = gitDir // configured later in IndexGitRepo
	buildOpts.RepositoryDescription.Branches = nil
	buildOpts.RepositoryDescription.SubRepoMap = nil
	buildOpts.RepositoryDescription.RawConfig = map[string]string{
		"priority": strconv.FormatFloat(0.0, 'g', -1, 64),
		"public":   marshalBool(true),
		"fork":     marshalBool(false),
		// Calculate repo rank based on the latest commit date.
		"latestCommitDate": marshalBool(true),
	}
	buildOpts.RepositoryDescription.Rank = 0
	// buildOpts.RepositoryDescription.IndexOptions = "" // configured later in IndexGitRepo
	// buildOpts.RepositoryDescription.LatestCommitDate = _ // configured later in IndexGitRepo
	buildOpts.RepositoryDescription.FileTombstones = nil

	gitOpts := gitindex.Options{
		RepoDir:                           gitDir,
		Submodules:                        false,
		Incremental:                       true,
		AllowMissingBranch:                false,
		RepoCacheDir:                      "",
		BuildOptions:                      buildOpts,
		BranchPrefix:                      "refs/heads/",
		Branches:                          branches,
		DeltaShardNumberFallbackThreshold: 0,
	}
	if _, err := gitindex.IndexGitRepo(gitOpts); err != nil {
		return err
	}
	return nil
}

func marshalBool(b bool) string {
	if b {
		return "1"
	}
	return "0"
}

func baseDir(plc string) identity.Directory {
	base := identity.BaseDirectory{
		PLCURL: plc,
		HTTPClient: http.Client{
			Timeout: time.Second * 10,
			Transport: &http.Transport{
				Proxy: http.ProxyFromEnvironment,
				// would want this around 100ms for services doing lots of handle resolution. Impacts PLC connections as well, but not too bad.
				IdleConnTimeout: time.Millisecond * 1000,
				MaxIdleConns:    100,
			},
		},
		Resolver: net.Resolver{
			Dial: func(ctx context.Context, network, address string) (net.Conn, error) {
				d := net.Dialer{Timeout: time.Second * 3}
				return d.DialContext(ctx, network, address)
			},
		},
		TryAuthoritativeDNS: true,
		// primary Bluesky PDS instance only supports HTTP resolution method
		SkipDNSDomainSuffixes: []string{".bsky.social"},
		UserAgent:             "indigo-identity/" + versioninfo.Short(),
	}
	return identity.NewCacheDirectory(&base, 250_000, time.Hour*24, time.Minute*2, time.Minute*5)
}
