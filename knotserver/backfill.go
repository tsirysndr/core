package knotserver

import (
	"context"
	"fmt"
	"log/slog"
	"maps"
	"slices"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
)

const (
	collaboratorBackfillMigration = "backfill-collaborators-from-casbin-v1"
	knotMemberBackfillMigration   = "backfill-knot-members-from-casbin-v1"
)

func BackfillKnotMembers(
	ctx context.Context,
	d *db.DB,
	e *rbac.Enforcer,
	ownerDid string,
	logger *slog.Logger,
) error {
	l := logger.With("migration", knotMemberBackfillMigration)

	applied, err := d.IsMigrationApplied(knotMemberBackfillMigration)
	if err != nil {
		return fmt.Errorf("check migration applied: %w", err)
	}
	if applied {
		return nil
	}

	owner, err := syntax.ParseDID(ownerDid)
	if err != nil {
		return fmt.Errorf("invalid knot owner DID %q: %w", ownerDid, err)
	}

	members, err := e.GetKnotUsersByRole("server:member", rbac.ThisServer)
	if err != nil {
		return fmt.Errorf("list members: %w", err)
	}
	owners, err := e.GetKnotUsersByRole("server:owner", rbac.ThisServer)
	if err != nil {
		return fmt.Errorf("list owners: %w", err)
	}

	var rows []db.KnotMember
	for _, candidate := range members {
		if slices.Contains(owners, candidate) {
			continue
		}
		subject, err := syntax.ParseDID(candidate)
		if err != nil {
			l.Warn("skipping member with invalid DID", "candidate", candidate, "err", err)
			continue
		}
		rows = append(rows, db.KnotMember{Did: owner, Subject: subject})
	}

	if err := d.ApplyKnotMemberBackfill(ctx, rows, knotMemberBackfillMigration); err != nil {
		return fmt.Errorf("apply backfill: %w", err)
	}

	l.Info("backfilled knot members from casbin", "count", len(rows))
	return nil
}

func BackfillCollaborators(
	ctx context.Context,
	d *db.DB,
	e *rbac.Enforcer,
	logger *slog.Logger,
	markApplied bool,
) error {
	l := logger.With("migration", collaboratorBackfillMigration)

	applied, err := d.IsMigrationApplied(collaboratorBackfillMigration)
	if err != nil {
		return fmt.Errorf("check migration applied: %w", err)
	}
	if applied {
		return nil
	}

	byRepo, err := e.GetCollaboratorsByRepo(rbac.ThisServer)
	if err != nil {
		return fmt.Errorf("list collaborators: %w", err)
	}

	var rows []db.Collaborator
	var skipped int
	for _, repoDid := range slices.Sorted(maps.Keys(byRepo)) {
		candidates := byRepo[repoDid]

		ownerDid, _, err := d.GetRepoKeyOwner(repoDid)
		if err != nil {
			l.Warn("skipping collaborators for unresolvable repo", "repoDid", repoDid, "collaborators", len(candidates), "err", err)
			skipped += len(candidates)
			continue
		}

		repo, err := syntax.ParseDID(repoDid)
		if err != nil {
			l.Warn("skipping collaborators for repo with invalid DID", "repoDid", repoDid, "collaborators", len(candidates), "err", err)
			skipped += len(candidates)
			continue
		}
		owner, err := syntax.ParseDID(ownerDid)
		if err != nil {
			l.Warn("skipping collaborators for repo with invalid owner DID", "repoDid", repoDid, "owner", ownerDid, "collaborators", len(candidates), "err", err)
			skipped += len(candidates)
			continue
		}

		for _, candidate := range candidates {
			subject, err := syntax.ParseDID(candidate)
			if err != nil {
				l.Warn("skipping collaborator with invalid DID", "repoDid", repoDid, "candidate", candidate, "err", err)
				skipped++
				continue
			}
			rows = append(rows, db.Collaborator{
				RepoDid: repo,
				Subject: subject,
				AddedBy: owner,
			})
		}
	}

	if err := d.ApplyCollaboratorBackfill(ctx, rows, collaboratorBackfillMigration, markApplied); err != nil {
		return fmt.Errorf("apply backfill: %w", err)
	}

	l.Info("backfilled collaborators from casbin", "count", len(rows), "repos", len(byRepo), "skipped", skipped, "marked", markApplied)
	return nil
}
