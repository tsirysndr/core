package knotserver

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"

	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
)

const (
	knotMembersBackfillMigration = "backfill-knot-members-from-pds-v2"
	knotMembersBackfillPerOwner  = 30 * time.Second
)

func BackfillKnotMembers(
	ctx context.Context,
	d *db.DB,
	e *rbac.Enforcer,
	resolver *idresolver.Resolver,
	hostname string,
	logger *slog.Logger,
) error {
	l := logger.With("migration", knotMembersBackfillMigration)

	applied, err := d.IsMigrationApplied(knotMembersBackfillMigration)
	if err != nil {
		return fmt.Errorf("check migration applied: %w", err)
	}
	if applied {
		return nil
	}

	owners, err := e.GetKnotUsersByRole("server:owner", rbac.ThisServer)
	if err != nil {
		return fmt.Errorf("list owners: %w", err)
	}

	var rows []db.KnotMember
	for _, owner := range owners {
		ownerCtx, cancel := context.WithTimeout(ctx, knotMembersBackfillPerOwner)
		ownerRows, err := fetchOwnerKnotMembers(ownerCtx, resolver, hostname, owner, l)
		cancel()
		if err != nil {
			l.Warn("skipping owner during backfill", "owner", owner, "err", err)
			continue
		}
		rows = append(rows, ownerRows...)
	}

	for _, m := range rows {
		if err := e.AddKnotMember(rbac.ThisServer, m.Subject.String()); err != nil {
			return fmt.Errorf("grant ACL for %s: %w", m.Subject, err)
		}
	}

	if err := d.ApplyKnotMembersBackfill(ctx, rows, knotMembersBackfillMigration); err != nil {
		return fmt.Errorf("apply backfill: %w", err)
	}

	l.Info("backfilled knot members", "count", len(rows), "owners", len(owners))
	return nil
}

func fetchOwnerKnotMembers(
	ctx context.Context,
	resolver *idresolver.Resolver,
	hostname string,
	owner string,
	l *slog.Logger,
) ([]db.KnotMember, error) {
	ownerDid, err := syntax.ParseDID(owner)
	if err != nil {
		return nil, fmt.Errorf("invalid owner DID %q: %w", owner, err)
	}

	ident, err := resolver.ResolveIdent(ctx, owner)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", owner, err)
	}
	client := &xrpc.Client{Host: ident.PDSEndpoint()}

	var (
		rows   []db.KnotMember
		cursor string
	)
	for {
		out, err := comatproto.RepoListRecords(ctx, client, tangled.KnotMemberNSID, cursor, 100, owner, false)
		if err != nil {
			return nil, fmt.Errorf("list records: %w", err)
		}
		for _, rec := range out.Records {
			m, ok := rec.Value.Val.(*tangled.KnotMember)
			if !ok || m.Domain != hostname {
				continue
			}
			subject, err := syntax.ParseDID(m.Subject)
			if err != nil {
				l.Warn("invalid subject DID in record, skipping", "uri", rec.Uri, "err", err)
				continue
			}
			uri, err := syntax.ParseATURI(rec.Uri)
			if err != nil {
				l.Warn("invalid AT URI in record, skipping", "uri", rec.Uri, "err", err)
				continue
			}
			rkey := uri.RecordKey().String()
			if rkey == "" {
				l.Warn("empty rkey in AT URI, skipping", "uri", rec.Uri)
				continue
			}
			rows = append(rows, db.KnotMember{
				Did:     ownerDid,
				Rkey:    rkey,
				Subject: subject,
			})
		}
		if out.Cursor == nil || *out.Cursor == "" || *out.Cursor == cursor {
			break
		}
		cursor = *out.Cursor
	}
	return rows, nil
}
