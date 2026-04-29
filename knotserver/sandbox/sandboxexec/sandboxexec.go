package sandboxexec

import (
	"context"
	"fmt"
	"os"

	"github.com/urfave/cli/v3"
)

// Command returns the hidden sandbox-exec subcommand used by LandlockBackend.
//
// landlock_restrict_self only restricts the calling OS thread, so it cannot be
// called from a goroutine (the Go scheduler may migrate the goroutine across
// threads). The workaround is to re-exec the knot binary with this subcommand,
// which runs single-threaded before the Go runtime starts its thread pool,
// applies the ruleset, then exec's into the target git process.
func Command() *cli.Command {
	return &cli.Command{
		Name:   "sandbox-exec",
		Hidden: true,
		Usage:  "apply landlock sandbox and exec into git (internal use only)",
		Action: Run,
		Flags: []cli.Flag{
			&cli.StringSliceFlag{
				Name:  "repo-path",
				Usage: "repository path(s) to allow read/write access to",
			},
		},
	}
}

func Run(ctx context.Context, cmd *cli.Command) error {
	repoPaths := cmd.StringSlice("repo-path")
	gitArgs := cmd.Args().Slice()

	if len(gitArgs) == 0 {
		fmt.Fprintln(os.Stderr, "sandbox-exec: no command specified after --")
		os.Exit(1)
	}

	return applyAndExec(repoPaths, gitArgs)
}
