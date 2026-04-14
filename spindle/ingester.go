package spindle

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/db"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"github.com/bluesky-social/jetstream/pkg/models"
	securejoin "github.com/cyphar/filepath-securejoin"
)

type Ingester func(ctx context.Context, e *models.Event) error

func (s *Spindle) ingest() Ingester {
	return func(ctx context.Context, e *models.Event) error {
		if e.Kind != models.EventKindCommit {
			return nil
		}

		var err error
		switch e.Commit.Collection {
		case tangled.SpindleMemberNSID:
			err = s.ingestMember(ctx, e)
		case tangled.RepoNSID:
			err = s.ingestRepo(ctx, e)
		case tangled.RepoCollaboratorNSID:
			err = s.ingestCollaborator(ctx, e)
		}

		if err != nil {
			s.l.Warn("failed to process message, skipping", "nsid", e.Commit.Collection, "err", err)
		}

		lastTimeUs := e.TimeUS + 1
		if saveErr := s.db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
			s.l.Error("failed to save cursor", "err", saveErr)
		}

		return nil
	}
}

func (s *Spindle) ingestMember(_ context.Context, e *models.Event) error {
	var err error
	did := e.Did
	rkey := e.Commit.RKey

	l := s.l.With("component", "ingester", "record", tangled.SpindleMemberNSID)

	switch e.Commit.Operation {
	case models.CommitOperationCreate, models.CommitOperationUpdate:
		raw := e.Commit.Record
		record := tangled.SpindleMember{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "error", err)
			return err
		}

		domain := s.cfg.Server.Hostname
		recordInstance := record.Instance

		if recordInstance != domain {
			l.Error("domain mismatch", "domain", recordInstance, "expected", domain)
			return fmt.Errorf("domain mismatch: %s != %s", record.Instance, domain)
		}

		ok, err := s.e.IsSpindleInviteAllowed(did, rbacDomain)
		if err != nil || !ok {
			l.Error("failed to add member", "did", did, "error", err)
			return fmt.Errorf("failed to enforce permissions: %w", err)
		}

		if err := db.AddSpindleMember(s.db, db.SpindleMember{
			Did:      syntax.DID(did),
			Rkey:     rkey,
			Instance: recordInstance,
			Subject:  syntax.DID(record.Subject),
			Created:  time.Now(),
		}); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}

		if err := s.e.AddSpindleMember(rbacDomain, record.Subject); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}
		l.Info("added member from firehose", "member", record.Subject)

		if err := s.db.AddDid(record.Subject); err != nil {
			l.Error("failed to add did", "error", err)
			return fmt.Errorf("failed to add did: %w", err)
		}
		s.jc.AddDid(record.Subject)

		return nil

	case models.CommitOperationDelete:
		record, err := db.GetSpindleMember(s.db, did, rkey)
		if err != nil {
			l.Error("failed to find member", "error", err)
			return fmt.Errorf("failed to find member: %w", err)
		}

		if err := db.RemoveSpindleMember(s.db, did, rkey); err != nil {
			l.Error("failed to remove member", "error", err)
			return fmt.Errorf("failed to remove member: %w", err)
		}

		if err := s.e.RemoveSpindleMember(rbacDomain, record.Subject.String()); err != nil {
			l.Error("failed to add member", "error", err)
			return fmt.Errorf("failed to add member: %w", err)
		}
		l.Info("added member from firehose", "member", record.Subject)

		if err := s.db.RemoveDid(record.Subject.String()); err != nil {
			l.Error("failed to add did", "error", err)
			return fmt.Errorf("failed to add did: %w", err)
		}
		s.jc.RemoveDid(record.Subject.String())

	}
	return nil
}

func (s *Spindle) ingestRepo(ctx context.Context, e *models.Event) error {
	var err error
	did := e.Did

	l := s.l.With("component", "ingester", "record", tangled.RepoNSID)

	l.Info("ingesting repo record", "did", did)

	switch e.Commit.Operation {
	case models.CommitOperationCreate, models.CommitOperationUpdate:
		raw := e.Commit.Record
		record := tangled.Repo{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "error", err)
			return err
		}

		domain := s.cfg.Server.Hostname
		rkey := e.Commit.RKey

		// no spindle configured for this repo
		if record.Spindle == nil {
			l.Info("no spindle configured", "rkey", rkey)
			return nil
		}

		// this repo did not want this spindle
		if *record.Spindle != domain {
			l.Info("different spindle configured", "rkey", rkey, "spindle", *record.Spindle, "domain", domain)
			return nil
		}

		// add this repo to the watch list
		if err := s.db.AddRepo(record.Knot, did, rkey); err != nil {
			l.Error("failed to add repo", "error", err)
			return fmt.Errorf("failed to add repo: %w", err)
		}

		didSlashRepo, err := securejoin.SecureJoin(did, rkey)
		if err != nil {
			return err
		}

		// add repo to rbac
		if err := s.e.AddRepo(did, rbac.ThisServer, didSlashRepo); err != nil {
			l.Error("failed to add repo to enforcer", "error", err)
			return fmt.Errorf("failed to add repo: %w", err)
		}

		// add collaborators to rbac
		owner, err := s.res.ResolveIdent(ctx, did)
		if err != nil || owner.Handle.IsInvalidHandle() {
			return err
		}
		if err := s.fetchAndAddCollaborators(ctx, owner, didSlashRepo); err != nil {
			return err
		}

		// add this knot to the event consumer
		src := eventconsumer.NewKnotSource(record.Knot)
		s.ks.AddSource(context.Background(), src)

		return nil

	}
	return nil
}

func (s *Spindle) ingestCollaborator(ctx context.Context, e *models.Event) error {
	var err error

	l := s.l.With("component", "ingester", "record", tangled.RepoCollaboratorNSID, "did", e.Did)

	l.Info("ingesting collaborator record")

	switch e.Commit.Operation {
	case models.CommitOperationCreate, models.CommitOperationUpdate:
		raw := e.Commit.Record
		record := tangled.RepoCollaborator{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "error", err)
			return err
		}

		subjectId, err := s.res.ResolveIdent(ctx, record.Subject)
		if err != nil || subjectId.Handle.IsInvalidHandle() {
			return err
		}

		var rbacResource string
		var ownerDid string
		switch {
		case strings.HasPrefix(record.Repo, "did:"):
			resolvedOwner, repoName, lookupErr := s.resolveRepoDid(ctx, e.Did, record.Repo)
			if lookupErr != nil {
				return fmt.Errorf("unknown repo DID %s: %w", record.Repo, lookupErr)
			}
			ownerDid = resolvedOwner
			rbacResource, _ = securejoin.SecureJoin(ownerDid, repoName)

		case strings.Contains(record.Repo, "/"):
			repoAt, parseErr := syntax.ParseATURI(record.Repo)
			if parseErr != nil {
				l.Info("rejecting record, invalid repoAt", "repoAt", record.Repo)
				return nil
			}

			owner, resolveErr := s.res.ResolveIdent(ctx, repoAt.Authority().String())
			if resolveErr != nil || owner.Handle.IsInvalidHandle() {
				return fmt.Errorf("failed to resolve handle: %w", resolveErr)
			}

			xrpcc := xrpc.Client{
				Host: owner.PDSEndpoint(),
			}

			resp, getErr := comatproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
			if getErr != nil {
				return getErr
			}

			if _, ok := resp.Value.Val.(*tangled.Repo); !ok {
				return fmt.Errorf("record at %s is not a tangled.Repo", repoAt)
			}
			rbacResource, _ = securejoin.SecureJoin(owner.DID.String(), repoAt.RecordKey().String())
			ownerDid = owner.DID.String()

		default:
			l.Info("rejecting collaborator record with unrecognized repo format", "repo", record.Repo)
			return nil
		}

		if ok, err := s.e.IsCollaboratorInviteAllowed(ownerDid, rbac.ThisServer, rbacResource); !ok || err != nil {
			return fmt.Errorf("insufficient permissions: %w", err)
		}

		if err := s.e.AddCollaborator(record.Subject, rbac.ThisServer, rbacResource); err != nil {
			l.Error("failed to add collaborator to enforcer", "error", err)
			return fmt.Errorf("failed to add collaborator: %w", err)
		}

		return nil
	}
	return nil
}

func (s *Spindle) resolveRepoDid(ctx context.Context, ownerDid string, repoDid string) (string, string, error) {
	owner, resolveErr := s.res.ResolveIdent(ctx, ownerDid)
	if resolveErr != nil || owner.Handle.IsInvalidHandle() {
		return "", "", fmt.Errorf("failed to resolve owner %s: %w", ownerDid, resolveErr)
	}

	xrpcc := xrpc.Client{
		Host: owner.PDSEndpoint(),
	}

	cursor := ""
	for {
		resp, listErr := comatproto.RepoListRecords(ctx, &xrpcc, tangled.RepoNSID, cursor, 100, ownerDid, false)
		if listErr != nil {
			return "", "", fmt.Errorf("failed to list repo records for %s: %w", ownerDid, listErr)
		}

		for _, r := range resp.Records {
			if r == nil {
				continue
			}
			repo, ok := r.Value.Val.(*tangled.Repo)
			if !ok {
				continue
			}
			if repo.RepoDid != nil && *repo.RepoDid == repoDid {
				rkey := r.Uri[strings.LastIndex(r.Uri, "/")+1:]
				return ownerDid, rkey, nil
			}
		}

		if resp.Cursor == nil || *resp.Cursor == "" {
			break
		}
		cursor = *resp.Cursor
	}

	return "", "", fmt.Errorf("repo DID %s not found in records for %s", repoDid, ownerDid)
}

func (s *Spindle) fetchAndAddCollaborators(ctx context.Context, owner *identity.Identity, didSlashRepo string) error {
	l := s.l.With("component", "ingester", "handler", "fetchAndAddCollaborators")

	l.Info("fetching and adding existing collaborators")

	xrpcc := xrpc.Client{
		Host: owner.PDSEndpoint(),
	}

	resp, err := comatproto.RepoListRecords(ctx, &xrpcc, tangled.RepoCollaboratorNSID, "", 50, owner.DID.String(), false)
	if err != nil {
		return err
	}

	var errs error
	for _, r := range resp.Records {
		if r == nil {
			continue
		}
		record := r.Value.Val.(*tangled.RepoCollaborator)

		if err := s.e.AddCollaborator(record.Subject, rbac.ThisServer, didSlashRepo); err != nil {
			l.Error("failed to add repo to enforcer", "error", err)
			errors.Join(errs, fmt.Errorf("failed to add repo: %w", err))
		}
	}

	return errs
}
