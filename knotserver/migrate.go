package knotserver

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"math/rand/v2"
	"os"
	"path/filepath"
	"strings"
	"time"

	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/repodid"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
)

type legacyRepo struct {
	ownerDid string
	repoName string
	oldPath  string
}

func scanLegacyRepos(scanPath string, logger *slog.Logger) []legacyRepo {
	topEntries, err := os.ReadDir(scanPath)
	if err != nil {
		logger.Error("reading scan path", "error", err)
		return nil
	}

	var repos []legacyRepo
	for _, entry := range topEntries {
		if !entry.IsDir() || !strings.HasPrefix(entry.Name(), "did:") {
			continue
		}

		ownerPath := filepath.Join(scanPath, entry.Name())

		if _, headErr := os.Stat(filepath.Join(ownerPath, "HEAD")); headErr == nil {
			continue
		}

		subEntries, readErr := os.ReadDir(ownerPath)
		if readErr != nil {
			logger.Error("reading owner dir", "ownerDir", ownerPath, "error", readErr)
			continue
		}

		ownerDid := entry.Name()
		for _, sub := range subEntries {
			if !sub.IsDir() {
				continue
			}
			subPath := filepath.Join(ownerPath, sub.Name())
			if _, statErr := os.Stat(filepath.Join(subPath, "HEAD")); statErr != nil {
				logger.Warn("skipping non-repo directory", "path", subPath)
				continue
			}
			repos = append(repos, legacyRepo{
				ownerDid: ownerDid,
				repoName: sub.Name(),
				oldPath:  subPath,
			})
		}
	}
	return repos
}

func migrateReposOnStartup(ctx context.Context, c *config.Config, d *db.DB, e *rbac.Enforcer, n *notifier.Notifier, logger *slog.Logger) bool {
	repos := scanLegacyRepos(c.Repo.ScanPath, logger)
	if len(repos) == 0 {
		logger.Info("no legacy repos found, migration complete")
		return true
	}

	logger.Info("starting legacy repo migration", "count", len(repos))
	start := time.Now()

	knotServiceUrl := "https://" + c.Server.Hostname
	if c.Server.Dev {
		knotServiceUrl = "http://" + c.Server.Hostname
	}

	migrated := 0
	for _, repo := range repos {
		select {
		case <-ctx.Done():
			logger.Info("migration interrupted by shutdown", "migrated", migrated, "remaining", len(repos)-migrated)
			return false
		default:
		}

		err := migrateOneRepo(ctx, c, d, e, n, logger, repo, knotServiceUrl)
		if err != nil {
			logger.Error("migration failed for repo", "owner", repo.ownerDid, "repo", repo.repoName, "error", err)
			continue
		}
		migrated++
	}

	logger.Info("legacy repo migration complete", "migrated", migrated, "total", len(repos), "duration", time.Since(start))
	return migrated == len(repos)
}

func migrateOneRepo(
	ctx context.Context,
	c *config.Config,
	d *db.DB,
	e *rbac.Enforcer,
	n *notifier.Notifier,
	logger *slog.Logger,
	repo legacyRepo,
	knotServiceUrl string,
) error {
	l := logger.With("owner", repo.ownerDid, "repo", repo.repoName)

	repoDid, err := d.GetRepoDid(repo.ownerDid, repo.repoName)
	needsMint := errors.Is(err, sql.ErrNoRows)
	if err != nil && !needsMint {
		return fmt.Errorf("checking repo_keys: %w", err)
	}

	if needsMint {
		repoDid, err = mintAndStoreRepoDID(ctx, c, d, l, repo, knotServiceUrl)
		if err != nil {
			return err
		}
	}

	if err := rewriteRBACPolicies(e, repo.ownerDid, repo.repoName, repoDid, l); err != nil {
		return fmt.Errorf("rewriting RBAC policies: %w", err)
	}

	newPath := filepath.Join(c.Repo.ScanPath, repoDid)

	if err := moveRepoOnDisk(repo.oldPath, newPath, l); err != nil {
		return err
	}

	ownerDir := filepath.Dir(repo.oldPath)
	if err := os.Remove(ownerDir); err != nil && !errors.Is(err, os.ErrNotExist) {
		l.Warn("could not remove empty owner dir", "path", ownerDir, "error", err)
	}

	if err := d.EmitDIDAssign(n, repo.ownerDid, repo.repoName, repoDid); err != nil {
		l.Error("emitting didAssign event failed (non-fatal)", "error", err)
	}

	l.Info("migrated repo", "repoDid", repoDid)
	return nil
}

func mintAndStoreRepoDID(
	ctx context.Context,
	c *config.Config,
	d *db.DB,
	l *slog.Logger,
	repo legacyRepo,
	knotServiceUrl string,
) (string, error) {
	prepared, err := repodid.PrepareRepoDID(c.Server.PlcUrl, knotServiceUrl)
	if err != nil {
		return "", fmt.Errorf("preparing DID: %w", err)
	}

	if err := submitWithBackoff(ctx, prepared, l); err != nil {
		return "", fmt.Errorf("PLC submission: %w", err)
	}

	if err := d.StoreRepoKey(prepared.RepoDid, prepared.SigningKeyRaw, repo.ownerDid, repo.repoName); err != nil {
		return "", fmt.Errorf("storing repo key: %w", err)
	}

	return prepared.RepoDid, nil
}

func submitWithBackoff(ctx context.Context, prepared *repodid.PreparedDID, l *slog.Logger) error {
	backoff := 2 * time.Second
	const maxBackoff = 5 * time.Minute
	const maxAttempts = 20

	for attempt := range maxAttempts {
		plcCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
		err := prepared.Submit(plcCtx)
		cancel()

		if err == nil {
			return nil
		}

		errMsg := err.Error()
		retryable := strings.Contains(errMsg, "429") ||
			strings.Contains(errMsg, "500") ||
			strings.Contains(errMsg, "502") ||
			strings.Contains(errMsg, "503") ||
			strings.Contains(errMsg, "504")

		if !retryable || attempt == maxAttempts-1 {
			return err
		}

		jitter := time.Duration(rand.Int64N(int64(backoff / 4)))
		delay := backoff + jitter

		l.Info("PLC rate limited, backing off", "delay", delay, "attempt", attempt+1)

		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(delay):
		}

		backoff = min(backoff*2, maxBackoff)
	}

	return fmt.Errorf("unreachable: exceeded max attempts")
}

func moveRepoOnDisk(oldPath, newPath string, l *slog.Logger) error {
	if _, err := os.Stat(newPath); err == nil {
		l.Info("new path already exists, skipping rename", "newPath", newPath)
		return nil
	}

	if _, err := os.Stat(oldPath); errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("old path %s does not exist and new path %s not found", oldPath, newPath)
	}

	if err := os.Rename(oldPath, newPath); err != nil {
		return fmt.Errorf("renaming %s -> %s: %w", oldPath, newPath, err)
	}

	return nil
}

func rewriteRBACPolicies(e *rbac.Enforcer, ownerDid, repoName, repoDid string, l *slog.Logger) error {
	oldResource := ownerDid + "/" + repoName

	policies, err := e.E.GetFilteredPolicy(1, rbac.ThisServer, oldResource)
	if err != nil {
		return fmt.Errorf("getting old policies: %w", err)
	}

	var addPolicies [][]string
	var removePolicies [][]string
	for _, p := range policies {
		removePolicies = append(removePolicies, p)
		newPolicy := make([]string, len(p))
		copy(newPolicy, p)
		newPolicy[2] = repoDid
		addPolicies = append(addPolicies, newPolicy)
	}

	if len(addPolicies) > 0 {
		if _, addErr := e.E.AddPolicies(addPolicies); addErr != nil {
			return fmt.Errorf("adding new policies: %w", addErr)
		}
	}

	if len(removePolicies) > 0 {
		if _, rmErr := e.E.RemovePolicies(removePolicies); rmErr != nil {
			return fmt.Errorf("removing old policies: %w", rmErr)
		}
	}

	l.Info("rewrote RBAC policies", "old", oldResource, "new", repoDid, "count", len(policies))
	return nil
}
