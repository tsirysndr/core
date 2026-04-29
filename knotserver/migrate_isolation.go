package knotserver

import (
	"context"
	"fmt"
	"os"
	"syscall"

	"github.com/urfave/cli/v3"
	"tangled.org/core/hook"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/sandbox"
	"tangled.org/core/log"
)

// MigrateIsolationCommand returns the CLI command for migrate-isolation.
func MigrateIsolationCommand() *cli.Command {
	return &cli.Command{
		Name:   "migrate-isolation",
		Usage:  "chown existing repos to their owner virtual UIDs and refresh hooks (run once before enabling --secure-mode)",
		Action: RunMigrateIsolation,
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:  "git-dir",
				Usage: "base directory for git repos",
				Value: "/home/git",
			},
			&cli.StringFlag{
				Name:  "db",
				Usage: "path to knotserver SQLite database",
				Value: "knotserver.db",
			},
			&cli.StringFlag{
				Name:  "internal-api",
				Usage: "internal API address for hook configuration",
				Value: "127.0.0.1:5444",
			},
			&cli.BoolFlag{
				Name:  "force",
				Usage: "re-run chown/chmod even on already-migrated repos",
			},
		},
	}
}

// RunMigrateIsolation iterates over all repos in the DB, assigns virtual UIDs
// to their owners, chowns the repo trees recursively, and records isolated_at.
func RunMigrateIsolation(ctx context.Context, cmd *cli.Command) error {
	logger := log.FromContext(ctx)
	logger = log.SubLogger(logger, "migrate-isolation")

	gitDir := cmd.String("git-dir")
	dbPath := cmd.String("db")
	internalApi := cmd.String("internal-api")

	serviceGid, err := sandbox.ServiceGid(gitDir)
	if err != nil {
		return fmt.Errorf("resolve service gid: %w", err)
	}

	hookCfg := hook.Config(
		hook.WithScanPath(gitDir),
		hook.WithInternalApi(internalApi),
	)

	d, err := db.Setup(ctx, dbPath)
	if err != nil {
		return fmt.Errorf("failed to open db: %w", err)
	}

	repos, err := d.AllReposForMigration(cmd.Bool("force"))
	if err != nil {
		return fmt.Errorf("failed to list repos: %w", err)
	}

	if len(repos) == 0 {
		logger.Info("no repos need migration")
		return nil
	}

	logger.Info("starting isolation migration", "total", len(repos))

	var migrated, skipped, failed int

	for _, repo := range repos {
		repoPath, _, _, err := d.ResolveRepoDIDOnDisk(gitDir, repo.RepoDID)
		if err != nil {
			logger.Error("repo not on disk, skipping",
				"repo_did", repo.RepoDID, "error", err)
			skipped++
			continue
		}

		ownerUID, err := d.GetOrAssignOwnerUID(repo.OwnerDID)
		if err != nil {
			logger.Error("failed to get/assign UID",
				"repo_did", repo.RepoDID, "owner_did", repo.OwnerDID, "error", err)
			failed++
			continue
		}

		// regenerate hooks first so the chown walk preserves their 0755 mode
		// via the executable-bit check in ChownRepoTree.
		if err := hook.SetupRepo(hookCfg, repoPath); err != nil {
			logger.Error("failed to set up hooks",
				"repo_did", repo.RepoDID, "path", repoPath, "error", err)
			failed++
			continue
		}

		if err := sandbox.ChmodRepoTree(repoPath); err != nil {
			logger.Error("chmod failed",
				"repo_did", repo.RepoDID, "path", repoPath, "error", err)
			failed++
			continue
		}

		if err := sandbox.ChownRepoTree(repoPath, int(ownerUID), int(serviceGid)); err != nil {
			// EPERM: process lacks cap_chown; log and continue rather than
			// aborting so all failures are reported.
			if isEPERM(err) {
				logger.Error("chown failed: process lacks cap_chown "+
					"(run as root or grant cap_chown to the binary)",
					"repo_did", repo.RepoDID, "path", repoPath, "uid", ownerUID)
			} else {
				logger.Error("chown failed",
					"repo_did", repo.RepoDID, "path", repoPath, "uid", ownerUID, "error", err)
			}
			failed++
			continue
		}

		if err := d.MarkRepoIsolated(repo.RepoDID); err != nil {
			logger.Error("failed to record isolated_at",
				"repo_did", repo.RepoDID, "error", err)
			failed++
			continue
		}

		logger.Info("migrated repo",
			"repo_did", repo.RepoDID, "owner_did", repo.OwnerDID, "uid", ownerUID, "path", repoPath)
		migrated++
	}

	logger.Info("migration complete",
		"migrated", migrated, "skipped", skipped, "failed", failed, "total", len(repos))

	if failed > 0 {
		return fmt.Errorf("%d repo(s) failed to migrate", failed)
	}
	return nil
}

func isEPERM(err error) bool {
	if pathErr, ok := err.(*os.PathError); ok {
		return pathErr.Err == syscall.EPERM
	}
	return false
}
