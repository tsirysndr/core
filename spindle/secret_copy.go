package spindle

import (
	"context"
	"errors"
	"fmt"

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
