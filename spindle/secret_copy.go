package spindle

import (
	"context"
	"errors"
	"fmt"
	"log/slog"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/secrets"
)

func copyRepoSecrets(ctx context.Context, mgr secrets.Manager, src, dst secrets.RepoIdentifier) (int, error) {
	cur, err := mgr.GetSecretsUnlocked(ctx, src)
	if err != nil {
		return 0, fmt.Errorf("get %s: %w", src, err)
	}
	var step func(remaining []secrets.UnlockedSecret, copied int) (int, error)
	step = func(remaining []secrets.UnlockedSecret, copied int) (int, error) {
		if len(remaining) == 0 {
			return copied, nil
		}
		s := remaining[0]
		addErr := mgr.AddSecret(ctx, secrets.UnlockedSecret{
			Repo:      dst,
			Key:       s.Key,
			Value:     s.Value,
			CreatedAt: s.CreatedAt,
			CreatedBy: s.CreatedBy,
		})
		switch {
		case addErr == nil:
			return step(remaining[1:], copied+1)
		case errors.Is(addErr, secrets.ErrKeyAlreadyPresent):
			return step(remaining[1:], copied)
		default:
			return copied, fmt.Errorf("add %s/%s: %w", dst, s.Key, addErr)
		}
	}
	return step(cur, 0)
}

func legacyKeyCandidates(owner syntax.DID, name string, rkey syntax.RecordKey) []string {
	o := owner.String()
	r := rkey.String()
	switch {
	case name == "" && r == "":
		return nil
	case name == "":
		return []string{o + "/" + r}
	case r == "" || name == r:
		return []string{o + "/" + name}
	default:
		return []string{o + "/" + name, o + "/" + r}
	}
}

func migrateLegacyRepoSecrets(ctx context.Context, d *db.DB, vault secrets.Manager, logger *slog.Logger, owner syntax.DID, name string, rkey syntax.RecordKey, repoDid syntax.DID) {
	candidates := legacyKeyCandidates(owner, name, rkey)
	if len(candidates) == 0 {
		return
	}
	flag := "legacy-secret-copy:" + repoDid.String() + ":" + rkey.String()
	var exists bool
	if err := d.QueryRowContext(ctx, `select exists (select 1 from migrations where name = ?)`, flag).Scan(&exists); err != nil {
		logger.Warn("legacy secret copy: check migration flag", "err", err)
		return
	}
	if exists {
		return
	}

	newID := secrets.RepoIdentifier(repoDid.String())
	var step func(remaining []string, copied int) (int, error)
	step = func(remaining []string, copied int) (int, error) {
		if len(remaining) == 0 {
			return copied, nil
		}
		oldID := secrets.RepoIdentifier(remaining[0])
		n, err := copyRepoSecrets(ctx, vault, oldID, newID)
		if err != nil {
			return copied, fmt.Errorf("copy %s -> %s: %w", oldID, newID, err)
		}
		return step(remaining[1:], copied+n)
	}
	total, err := step(candidates, 0)
	if err != nil {
		logger.Warn("legacy secret copy failed", "err", err)
		return
	}

	if _, err := d.ExecContext(ctx, `insert or ignore into migrations (name) values (?)`, flag); err != nil {
		logger.Warn("legacy secret copy: mark flag failed", "err", err)
		return
	}
	logger.Info("legacy secret migrate done", "owner", owner, "name", name, "rkey", rkey, "repoDid", repoDid, "candidates", candidates, "copied", total)
}
