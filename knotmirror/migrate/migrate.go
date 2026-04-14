package migrate

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
)

type Stats struct {
	Renamed       int
	Skipped       int
	Orphaned      int
	OwnerDirsRm   int
	AlreadyExists int
}

func (s Stats) String() string {
	return fmt.Sprintf(
		"renamed=%d skipped=%d orphaned=%d owner_dirs_removed=%d target_existed=%d",
		s.Renamed, s.Skipped, s.Orphaned, s.OwnerDirsRm, s.AlreadyExists,
	)
}

func RenameDisk(ctx context.Context, base string, database *sql.DB, logger *slog.Logger) (Stats, error) {
	entries, err := os.ReadDir(base)
	if err != nil {
		return Stats{}, fmt.Errorf("reading base path: %w", err)
	}
	return reduceEntries(ctx, entries, 0, Stats{}, ownerStep(base, database, logger))
}

type stepFn func(context.Context, os.DirEntry, Stats) (Stats, error)

func reduceEntries(ctx context.Context, entries []os.DirEntry, idx int, acc Stats, fn stepFn) (Stats, error) {
	if idx >= len(entries) {
		return acc, nil
	}
	if err := ctx.Err(); err != nil {
		return acc, err
	}
	next, err := fn(ctx, entries[idx], acc)
	if err != nil {
		return next, err
	}
	return reduceEntries(ctx, entries, idx+1, next, fn)
}

func ownerStep(base string, database *sql.DB, logger *slog.Logger) stepFn {
	return func(ctx context.Context, entry os.DirEntry, acc Stats) (Stats, error) {
		if !entry.IsDir() || !strings.HasPrefix(entry.Name(), "did:") {
			return acc, nil
		}
		ownerPath := filepath.Join(base, entry.Name())
		if _, err := os.Stat(filepath.Join(ownerPath, "HEAD")); err == nil {
			return acc, nil
		}
		subEntries, err := os.ReadDir(ownerPath)
		if err != nil {
			logger.Error("reading owner dir", "ownerPath", ownerPath, "err", err)
			return acc, nil
		}
		next, err := reduceEntries(ctx, subEntries, 0, acc, rkeyStep(base, database, logger, syntax.DID(entry.Name()), ownerPath))
		if err != nil {
			return next, err
		}
		remaining, err := os.ReadDir(ownerPath)
		if err == nil && len(remaining) == 0 {
			if rmErr := os.Remove(ownerPath); rmErr == nil {
				next.OwnerDirsRm++
				logger.Info("removed empty owner dir", "ownerPath", ownerPath)
			} else {
				logger.Warn("failed to remove empty owner dir", "ownerPath", ownerPath, "err", rmErr)
			}
		}
		return next, nil
	}
}

func rkeyStep(base string, database *sql.DB, logger *slog.Logger, ownerDid syntax.DID, ownerPath string) stepFn {
	return func(ctx context.Context, sub os.DirEntry, acc Stats) (Stats, error) {
		if !sub.IsDir() {
			return acc, nil
		}
		rkey := sub.Name()
		subPath := filepath.Join(ownerPath, rkey)
		l := logger.With("did", ownerDid, "rkey", rkey, "subPath", subPath)

		if _, err := os.Stat(filepath.Join(subPath, "HEAD")); err != nil {
			l.Warn("skipping non-repo subdir")
			acc.Skipped++
			return acc, nil
		}

		aturi := syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", ownerDid, tangled.RepoNSID, rkey))
		repo, err := db.GetRepoByAtUri(ctx, database, aturi)
		if err != nil {
			return acc, fmt.Errorf("looking up repo by aturi %s: %w", aturi, err)
		}
		if repo == nil {
			l.Warn("orphan disk repo, no DB row; leaving in place")
			acc.Orphaned++
			return acc, nil
		}
		if repo.RepoDid == "" {
			l.Warn("DB row has empty repo_did; leaving in place")
			acc.Orphaned++
			return acc, nil
		}

		target := filepath.Join(base, repo.RepoDid.String())
		if _, err := os.Stat(target); err == nil {
			l.Warn("target path already exists; leaving source in place", "target", target)
			acc.AlreadyExists++
			return acc, nil
		} else if !errors.Is(err, os.ErrNotExist) {
			return acc, fmt.Errorf("stat target %s: %w", target, err)
		}

		if err := os.Rename(subPath, target); err != nil {
			return acc, fmt.Errorf("rename %s -> %s: %w", subPath, target, err)
		}
		acc.Renamed++
		l.Info("renamed", "target", target)
		return acc, nil
	}
}
