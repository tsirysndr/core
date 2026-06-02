package knotserver

import (
	"context"
	"fmt"
	"log/slog"
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

	repoDids, err := d.ListRepoDids()
	if err != nil {
		return fmt.Errorf("list repos: %w", err)
	}

	var rows []db.Collaborator
	for _, repoDid := range repoDids {
		ownerDid, _, err := d.GetRepoKeyOwner(repoDid)
		if err != nil {
			l.Warn("skipping repo during collaborator backfill", "repoDid", repoDid, "err", err)
			continue
		}

		repo, err := syntax.ParseDID(repoDid)
		if err != nil {
			l.Warn("skipping repo with invalid DID", "repoDid", repoDid, "err", err)
			continue
		}
		owner, err := syntax.ParseDID(ownerDid)
		if err != nil {
			l.Warn("skipping repo with invalid owner DID", "repoDid", repoDid, "owner", ownerDid, "err", err)
			continue
		}

		collaborators, err := e.GetUserByRoleInRepo("repo:collaborator", rbac.ThisServer, repoDid)
		if err != nil {
			return fmt.Errorf("list collaborators for %s: %w", repoDid, err)
		}

		for _, candidate := range collaborators {
			subject, err := syntax.ParseDID(candidate)
			if err != nil {
				l.Warn("skipping collaborator with invalid DID", "repoDid", repoDid, "candidate", candidate, "err", err)
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

	l.Info("backfilled collaborators from casbin", "count", len(rows), "repos", len(repoDids), "marked", markApplied)
	return nil
}
