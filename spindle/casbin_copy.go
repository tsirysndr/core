package spindle

import (
	"context"
	"fmt"
	"log/slog"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/db"
)

func migrateLegacyRepoCasbin(ctx context.Context, d *db.DB, e *rbac.Enforcer, logger *slog.Logger, owner syntax.DID, name string, rkey syntax.RecordKey, repoDid syntax.DID) {
	candidates := legacyKeyCandidates(owner, name, rkey)
	if siblings, err := d.SiblingRkeysForRepoDid(owner, repoDid, rkey); err == nil {
		var fold func(rest []string, acc []string) []string
		fold = func(rest []string, acc []string) []string {
			if len(rest) == 0 {
				return acc
			}
			return fold(rest[1:], append(acc, owner.String()+"/"+rest[0]))
		}
		candidates = fold(siblings, candidates)
	} else {
		logger.Warn("legacy casbin rekey: sibling lookup failed", "err", err)
	}
	if len(candidates) == 0 {
		return
	}
	flag := "legacy-casbin-rekey:" + repoDid.String() + ":" + rkey.String()
	var exists bool
	if err := d.QueryRowContext(ctx, `select exists (select 1 from migrations where name = ?)`, flag).Scan(&exists); err != nil {
		logger.Warn("legacy casbin rekey: check migration flag", "err", err)
		return
	}
	if exists {
		return
	}

	if err := e.AddRepo(owner.String(), rbac.ThisServer, repoDid.String()); err != nil {
		logger.Warn("legacy casbin rekey: owner add new key failed", "err", err)
		return
	}

	collabs, err := d.ListCollaboratorsByRepoDid(repoDid)
	if err != nil {
		logger.Warn("legacy casbin rekey: list collaborators failed", "err", err)
		return
	}

	var addCollabs func(remaining []db.RepoCollaborator) error
	addCollabs = func(remaining []db.RepoCollaborator) error {
		if len(remaining) == 0 {
			return nil
		}
		c := remaining[0]
		if err := e.AddCollaborator(c.Subject.String(), rbac.ThisServer, repoDid.String()); err != nil {
			return fmt.Errorf("AddCollaborator %s -> %s: %w", c.Subject, repoDid, err)
		}
		return addCollabs(remaining[1:])
	}
	if err := addCollabs(collabs); err != nil {
		logger.Warn("legacy casbin rekey: collaborator add failed", "err", err)
		return
	}

	var wipeCandidates func(remaining []string) error
	wipeCandidates = func(remaining []string) error {
		if len(remaining) == 0 {
			return nil
		}
		if err := e.WipeRepoPolicies(rbac.ThisServer, remaining[0]); err != nil {
			return fmt.Errorf("WipeRepoPolicies %s: %w", remaining[0], err)
		}
		return wipeCandidates(remaining[1:])
	}
	if err := wipeCandidates(candidates); err != nil {
		logger.Warn("legacy casbin rekey: wipe failed", "err", err)
		return
	}

	if _, err := d.ExecContext(ctx, `insert or ignore into migrations (name) values (?)`, flag); err != nil {
		logger.Warn("legacy casbin rekey: mark flag failed", "err", err)
		return
	}
	logger.Info("legacy casbin rekeyed", "owner", owner, "name", name, "rkey", rkey, "repoDid", repoDid, "candidates", candidates, "collabs", len(collabs))
}
