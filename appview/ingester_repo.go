package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
	"tangled.org/core/repoident"
)

func (i *Ingester) ingestRepo(ctx context.Context, e *jmodels.Event, l *slog.Logger) error {
	l = l.With("handler", "ingestRepo")

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		return i.ingestRepoCreate(ctx, e, l)
	case jmodels.CommitOperationUpdate:
		return i.ingestRepoUpdate(ctx, e, l)
	case jmodels.CommitOperationDelete:
		return i.ingestRepoDelete(ctx, e, l)
	default:
		l.Info("unknown repo operation")
		return nil
	}
}

func (i *Ingester) ingestRepoCreate(ctx context.Context, e *jmodels.Event, l *slog.Logger) error {
	l = l.With("handler", "ingestRepoCreate")

	record := tangled.Repo{}
	if err := json.Unmarshal(json.RawMessage(e.Commit.Record), &record); err != nil {
		l.Error("invalid record", "err", err)
		return err
	}

	if record.RepoDid == nil || *record.RepoDid == "" {
		l.Info("skipping repo create from non-DID-migrated knot")
		return nil
	}
	repoDid := *record.RepoDid

	proceed, err := i.verifyOwnership(ctx, l, repoDid, e.Did, record.Knot)
	if err != nil {
		return err
	}
	if !proceed {
		return nil
	}

	existing, err := db.GetRepo(i.Db,
		orm.FilterEq("did", e.Did),
		orm.FilterEq("rkey", e.Commit.RKey),
	)
	if err == nil {
		l.Info("repo row already exists, skipping create", "did", e.Did, "rkey", e.Commit.RKey)
		if err := i.ensureRepoOwnerPermissions(e.Did, existing.Knot, existing.RepoIdentifier()); err != nil {
			return fmt.Errorf("failed to ensure repo owner permissions: %w", err)
		}
		return nil
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return fmt.Errorf("failed to check existing repo: %w", err)
	}

	prev, err := db.GetRepoByDid(i.Db, repoDid)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return fmt.Errorf("failed to check existing repoDid: %w", err)
	}

	if prev != nil {
		l.Info("repoDid exists under different rkey, renaming",
			"oldRkey", prev.Rkey, "newRkey", e.Commit.RKey)

		oldRepo := *prev

		tx, txErr := i.Db.Begin()
		if txErr != nil {
			return fmt.Errorf("failed to begin rename tx: %w", txErr)
		}
		defer tx.Rollback()

		newName := derefString(record.Name)
		if newName == "" {
			newName = e.Commit.RKey
		}

		if err := db.RenameRepo(tx, e.Did, prev.Rkey, e.Commit.RKey, newName); err != nil {
			return fmt.Errorf("failed to rename repo: %w", err)
		}
		if err := db.RecordRepoRename(tx, e.Did, prev.Rkey, repoDid); err != nil {
			return fmt.Errorf("failed to record rename history: %w", err)
		}
		if err := db.DeleteRepoRename(tx, e.Did, strings.ToLower(newName)); err != nil {
			return fmt.Errorf("failed to clear colliding rename alias: %w", err)
		}

		renamed := *prev
		renamed.Rkey = e.Commit.RKey
		renamed.Name = newName
		desired := repoFromRecord(&renamed, &record)
		if repoMetadataChanged(&renamed, &desired) {
			if err := applyRepoMetadata(tx, &renamed, desired); err != nil {
				return fmt.Errorf("failed to apply metadata after rename: %w", err)
			}
		}

		if err := tx.Commit(); err != nil {
			return fmt.Errorf("failed to commit rename tx: %w", err)
		}

		newRepo, err := db.GetRepo(i.Db,
			orm.FilterEq("did", e.Did),
			orm.FilterEq("rkey", e.Commit.RKey),
		)
		if err != nil {
			l.Warn("failed to fetch repo after rename for notification", "err", err)
			return nil
		}
		if err := i.ensureRepoOwnerPermissions(e.Did, newRepo.Knot, newRepo.RepoIdentifier()); err != nil {
			return fmt.Errorf("failed to ensure repo owner permissions: %w", err)
		}
		i.Notifier.RenameRepo(ctx, syntax.DID(e.Did), &oldRepo, newRepo)
		return nil
	}

	rkey := e.Commit.RKey
	name := derefString(record.Name)
	if name == "" {
		name = rkey
	}

	repo := &models.Repo{
		Did:         e.Did,
		Name:        name,
		Knot:        record.Knot,
		Rkey:        rkey,
		Description: derefString(record.Description),
		Website:     derefString(record.Website),
		Topics:      append([]string(nil), record.Topics...),
		Source:      derefString(record.Source),
		Spindle:     derefString(record.Spindle),
		Labels:      append([]string(nil), record.Labels...),
		RepoDid:     repoDid,
	}

	tx, err := i.Db.Begin()
	if err != nil {
		return fmt.Errorf("failed to begin insert tx: %w", err)
	}
	defer tx.Rollback()

	if err := db.AddRepo(tx, repo); err != nil {
		return fmt.Errorf("failed to insert repo: %w", err)
	}
	if err := db.DeleteRepoRename(tx, e.Did, strings.ToLower(repo.Slug())); err != nil {
		return fmt.Errorf("failed to clear colliding rename alias: %w", err)
	}
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("failed to commit insert tx: %w", err)
	}

	if err := i.ensureRepoOwnerPermissions(e.Did, repo.Knot, repo.RepoIdentifier()); err != nil {
		return fmt.Errorf("failed to ensure repo owner permissions: %w", err)
	}

	i.Notifier.NewRepo(ctx, repo)
	return nil
}

func (i *Ingester) ensureRepoOwnerPermissions(ownerDid, knot, repo string) error {
	if i.Enforcer == nil {
		return fmt.Errorf("ingester has no RBAC enforcer configured")
	}
	if err := i.Enforcer.AddRepo(ownerDid, knot, repo); err != nil {
		return err
	}
	return i.Enforcer.E.SavePolicy()
}

func (i *Ingester) ingestRepoUpdate(ctx context.Context, e *jmodels.Event, l *slog.Logger) error {
	l = l.With("handler", "ingestRepoUpdate")

	record := tangled.Repo{}
	if err := json.Unmarshal(json.RawMessage(e.Commit.Record), &record); err != nil {
		l.Error("invalid record", "err", err)
		return err
	}

	if record.RepoDid == nil || *record.RepoDid == "" {
		l.Info("skipping repo update from non-DID-migrated knot")
		return nil
	}

	proceed, err := i.verifyOwnership(ctx, l, *record.RepoDid, e.Did, record.Knot)
	if err != nil {
		return err
	}
	if !proceed {
		return nil
	}

	current, err := db.GetRepo(i.Db,
		orm.FilterEq("did", e.Did),
		orm.FilterEq("rkey", e.Commit.RKey),
	)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			l.Info("skipping repo update for unknown row")
			return nil
		}
		return fmt.Errorf("failed to fetch repo for ingest: %w", err)
	}

	if current.RepoDid != "" && current.RepoDid != *record.RepoDid {
		l.Warn("rejecting repo update: repoDid is immutable",
			"currentRepoDid", current.RepoDid,
			"recordRepoDid", *record.RepoDid,
		)
		return nil
	}

	desired := repoFromRecord(current, &record)

	if current.Source != desired.Source {
		l.Warn("source field changed but mutation is unsupported, ignoring",
			"current", current.Source, "desired", desired.Source)
	}

	if !repoMetadataChanged(current, &desired) {
		return nil
	}

	tx, err := i.Db.Begin()
	if err != nil {
		return fmt.Errorf("failed to begin tx: %w", err)
	}
	defer tx.Rollback()

	if err := applyRepoMetadata(tx, current, desired); err != nil {
		return fmt.Errorf("failed to apply repo metadata: %w", err)
	}
	if err := tx.Commit(); err != nil {
		return err
	}

	return nil
}

func (i *Ingester) ingestRepoDelete(ctx context.Context, e *jmodels.Event, l *slog.Logger) error {
	l = l.With("handler", "ingestRepoDelete")

	repo, err := db.GetRepo(i.Db,
		orm.FilterEq("did", e.Did),
		orm.FilterEq("rkey", e.Commit.RKey),
	)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			l.Info("skipping repo delete for unknown row")
			return nil
		}
		return fmt.Errorf("failed to fetch repo for delete: %w", err)
	}

	if i.Enforcer == nil {
		return fmt.Errorf("ingester has no RBAC enforcer configured")
	}

	tx, err := i.Db.Begin()
	if err != nil {
		return fmt.Errorf("failed to start txn: %w", err)
	}
	committed := false
	defer func() {
		if committed {
			return
		}
		tx.Rollback()
		i.Enforcer.E.LoadPolicy()
	}()

	if err := db.RemoveRepo(tx, e.Did, e.Commit.RKey); err != nil {
		return fmt.Errorf("failed to delete repo: %w", err)
	}

	if err := i.Enforcer.WipeRepoPolicies(repo.Knot, repo.RepoIdentifier()); err != nil {
		return fmt.Errorf("failed to wipe repo permissions: %w", err)
	}

	if err := tx.Commit(); err != nil {
		return fmt.Errorf("failed to commit txn: %w", err)
	}

	if err := i.Enforcer.E.SavePolicy(); err != nil {
		return fmt.Errorf("failed to save ACLs: %w", err)
	}
	committed = true

	i.Notifier.DeleteRepo(ctx, repo)
	l.Info("deleted repo row")
	return nil
}

func applyRepoMetadata(tx *sql.Tx, current *models.Repo, desired models.Repo) error {
	if err := db.PutRepo(tx, desired); err != nil {
		return err
	}

	if current.Spindle != desired.Spindle {
		var spindlePtr *string
		if desired.Spindle != "" {
			spindlePtr = &desired.Spindle
		}
		if err := db.UpdateSpindle(tx, desired.RepoDid, spindlePtr); err != nil {
			return err
		}
	}

	if !labelsEqual(current.Labels, desired.Labels) {
		if err := reconcileLabels(tx, current, desired); err != nil {
			return err
		}
	}

	return nil
}

func reconcileLabels(tx *sql.Tx, current *models.Repo, desired models.Repo) error {
	added := filterOut(desired.Labels, current.Labels)
	removed := filterOut(current.Labels, desired.Labels)

	if err := applyEach(added, func(l string) error {
		return db.SubscribeLabel(tx, &models.RepoLabel{
			RepoDid: syntax.DID(desired.RepoDid),
			LabelAt: syntax.ATURI(l),
		})
	}); err != nil {
		return err
	}

	return applyEach(removed, func(l string) error {
		return db.UnsubscribeLabel(tx,
			orm.FilterEq("repo_did", desired.RepoDid),
			orm.FilterEq("label_at", l),
		)
	})
}

func filterOut(items, exclude []string) []string {
	return slices.DeleteFunc(slices.Clone(items), func(s string) bool {
		return slices.Contains(exclude, s)
	})
}

func applyEach(items []string, fn func(string) error) error {
	for _, item := range items {
		if err := fn(item); err != nil {
			return err
		}
	}
	return nil
}

func labelsEqual(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	aSorted := append([]string(nil), a...)
	bSorted := append([]string(nil), b...)
	slices.Sort(aSorted)
	slices.Sort(bSorted)
	return slices.Equal(aSorted, bSorted)
}

func repoFromRecord(current *models.Repo, record *tangled.Repo) models.Repo {
	out := *current
	out.Name = derefString(record.Name)
	if out.Name == "" {
		out.Name = current.Rkey
	}
	out.Knot = record.Knot
	out.Description = derefString(record.Description)
	out.Website = derefString(record.Website)
	out.Topics = append([]string(nil), record.Topics...)
	out.Spindle = derefString(record.Spindle)
	out.Source = derefString(record.Source)
	out.Labels = append([]string(nil), record.Labels...)
	if record.RepoDid != nil {
		out.RepoDid = *record.RepoDid
	}
	return out
}

func repoMetadataChanged(current *models.Repo, desired *models.Repo) bool {
	return current.Name != desired.Name ||
		current.Knot != desired.Knot ||
		current.Description != desired.Description ||
		current.Website != desired.Website ||
		current.TopicStr() != desired.TopicStr() ||
		current.Spindle != desired.Spindle ||
		!labelsEqual(current.Labels, desired.Labels)
}

func derefString(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

func (i *Ingester) verifyOwnership(ctx context.Context, l *slog.Logger, repoDid, eventDid, recordKnot string) (bool, error) {
	if i.Verifier == nil {
		return false, fmt.Errorf("ingester has no repo ownership verifier configured")
	}
	rd, err := repoident.NewRepoDid(repoDid)
	if err != nil {
		l.Warn("rejecting repo event: invalid repoDid on record", "repoDid", repoDid, "err", err)
		return false, nil
	}
	result, err := i.Verifier(ctx, rd)
	if err != nil {
		return false, fmt.Errorf("verify repo ownership: %w", err)
	}
	if result.OwnerDid == "" {
		l.Warn("knot lacks RepoDescribeRepo, skipping owner check; upgrade knot to 1.14+",
			"repoDid", repoDid, "knot", result.KnotURL.String())
	} else if result.OwnerDid.String() != eventDid {
		l.Warn("rejecting repo event: owner mismatch",
			"repoDid", repoDid,
			"claimedOwner", eventDid,
			"knotOwner", result.OwnerDid.String(),
			"knot", result.KnotURL.String(),
		)
		return false, nil
	}
	if !strings.EqualFold(recordKnot, result.KnotURL.Host) {
		l.Warn("rejecting repo event: record knot does not match DID-doc endpoint",
			"repoDid", repoDid,
			"recordKnot", recordKnot,
			"canonicalKnot", result.KnotURL.Host,
		)
		return false, nil
	}
	return true, nil
}
