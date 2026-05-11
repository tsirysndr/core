package spindle

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/log"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/db"
	"tangled.org/core/tapc"
)

const (
	maxPendingPerRepo = 64
	pendingCollabTTL  = 10 * time.Minute
)

type pendingCollabEvent struct {
	evt *tapc.RecordEventData
	at  time.Time
}

type Tap struct {
	logger         *slog.Logger
	spindle        *Spindle
	tap            tapc.Client
	pendingMu      sync.Mutex
	pendingCollabs map[syntax.DID][]pendingCollabEvent
}

func NewTapClient(s *Spindle) *Tap {
	return &Tap{
		logger:         log.SubLogger(s.l, "tapclient"),
		spindle:        s,
		tap:            tapc.NewClient(s.cfg.Server.Tap.Url, s.cfg.Server.Tap.AdminPassword),
		pendingCollabs: make(map[syntax.DID][]pendingCollabEvent),
	}
}

func (t *Tap) AddOwnerDIDs(ctx context.Context, dids []syntax.DID) error {
	if len(dids) == 0 {
		return nil
	}
	return t.tap.AddRepos(ctx, dids)
}

func (t *Tap) Start(ctx context.Context) {
	go t.tap.Connect(ctx, &tapc.SimpleIndexer{
		EventHandler:   t.processEvent,
		ConnectHandler: t.onConnect,
	})
	go t.purgePendingCollabsLoop(ctx)
}

func (t *Tap) onConnect(ctx context.Context) {
	t.spindle.declareTapInterest(ctx)
}

func (t *Tap) processEvent(ctx context.Context, evt tapc.Event) error {
	if evt.Type != tapc.EvtRecord || evt.Record == nil {
		return nil
	}
	switch evt.Record.Collection.String() {
	case tangled.RepoNSID:
		return t.processRepo(ctx, evt.Record)
	case tangled.RepoCollaboratorNSID:
		return t.processCollaborator(ctx, evt.Record)
	}
	return nil
}

func (t *Tap) processRepo(ctx context.Context, evt *tapc.RecordEventData) error {
	l := t.logger.With("collection", tangled.RepoNSID, "did", evt.Did, "rkey", evt.Rkey)

	ownerDid := evt.Did
	rkey := evt.Rkey

	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.Repo{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			l.Warn("skipping invalid repo record", "err", err)
			return nil
		}

		hostname := t.spindle.cfg.Server.Hostname
		prior, priorErr := t.spindle.db.GetRepoByOwnerRkey(ownerDid, rkey)
		knownRepo := priorErr == nil

		if record.Spindle == nil || *record.Spindle != hostname {
			if knownRepo {
				l.Info("tearing down repo reassigned from this spindle", "newSpindle", record.Spindle)
				return t.teardownRepo(l, prior, ownerDid, rkey)
			}
			return nil
		}

		if record.RepoDid == nil || *record.RepoDid == "" {
			l.Warn("skipping repo record without repoDid")
			return nil
		}
		repoDid, err := syntax.ParseDID(*record.RepoDid)
		if err != nil {
			l.Warn("skipping repo record with malformed repoDid", "value", *record.RepoDid, "err", err)
			return nil
		}

		if err := t.spindle.e.AddRepo(ownerDid.String(), rbac.ThisServer, repoDid.String()); err != nil {
			l.Error("failed to add repo policy", "err", err)
			return fmt.Errorf("add repo policy: %w", err)
		}

		src := eventconsumer.NewKnotSource(record.Knot)
		t.spindle.ks.AddSource(t.spindle.rootCtx, src)

		if err := t.spindle.db.AddRepo(db.Repo{
			Knot:      record.Knot,
			Owner:     ownerDid,
			Rkey:      rkey,
			RepoDid:   repoDid,
			CreatedAt: record.CreatedAt,
		}); err != nil {
			l.Error("failed to add repo row", "err", err)
			return fmt.Errorf("add repo: %w", err)
		}

		if removed, err := t.spindle.db.CollapseRepoSiblings(ownerDid, repoDid); err != nil {
			l.Warn("collapse rename siblings failed", "err", err)
		} else if removed > 0 {
			l.Info("collapsed rename leftovers", "owner", ownerDid, "repo_did", repoDid, "removed", removed)
		}

		if err := t.tap.AddRepos(ctx, []syntax.DID{ownerDid}); err != nil {
			l.Warn("tap AddRepos rejected", "did", ownerDid, "err", err)
		}

		t.drainPendingCollabs(ctx, repoDid)

	case tapc.RecordDeleteAction:
		repo, err := t.spindle.db.GetRepoByOwnerRkey(ownerDid, rkey)
		if err != nil {
			l.Info("skipping delete for unknown repo")
			return nil
		}
		return t.teardownRepo(l, repo, ownerDid, rkey)
	}
	return nil
}

func (t *Tap) teardownRepo(l *slog.Logger, repo *db.Repo, ownerDid syntax.DID, rkey syntax.RecordKey) error {
	if repo.RepoDid != "" {
		collabs, err := t.spindle.db.ListCollaboratorsByRepoDid(repo.RepoDid)
		if err != nil {
			l.Error("failed to list collaborators for cleanup", "err", err)
			return fmt.Errorf("list collaborators: %w", err)
		}
		for _, c := range collabs {
			if err := t.spindle.e.RemoveCollaborator(c.Subject.String(), rbac.ThisServer, repo.RepoDid.String()); err != nil {
				l.Error("failed to remove collaborator policy", "subject", c.Subject, "err", err)
				return fmt.Errorf("remove collaborator policy: %w", err)
			}
		}
		if err := t.spindle.db.DeleteRepoCollaboratorsByRepoDid(repo.RepoDid); err != nil {
			l.Error("failed to clear collaborator rows", "err", err)
			return err
		}
		if err := t.spindle.e.RemoveRepo(ownerDid.String(), rbac.ThisServer, repo.RepoDid.String()); err != nil {
			l.Error("failed to remove repo policy", "err", err)
			return fmt.Errorf("remove repo policy: %w", err)
		}
	}
	if err := t.spindle.db.DeleteRepoByOwnerRkey(ownerDid, rkey); err != nil {
		l.Error("failed to delete repo row", "err", err)
		return fmt.Errorf("delete repo row: %w", err)
	}
	return nil
}

func (t *Tap) processCollaborator(ctx context.Context, evt *tapc.RecordEventData) error {
	l := t.logger.With("collection", tangled.RepoCollaboratorNSID, "did", evt.Did, "rkey", evt.Rkey)

	switch evt.Action {
	case tapc.RecordCreateAction, tapc.RecordUpdateAction:
		record := tangled.RepoCollaborator{}
		if err := json.Unmarshal(evt.Record, &record); err != nil {
			l.Warn("skipping invalid collaborator record", "err", err)
			return nil
		}

		actor := evt.Did
		rkey := evt.Rkey

		subjectDid, err := syntax.ParseDID(record.Subject)
		if err != nil {
			l.Info("skipping collaborator with malformed subject DID", "subject", record.Subject, "err", err)
			return nil
		}
		if _, err := t.spindle.res.ResolveIdent(ctx, subjectDid.String()); err != nil {
			l.Info("skipping unresolvable collaborator subject", "subject", subjectDid, "err", err)
			return nil
		}

		repoRefDid, err := syntax.ParseDID(record.Repo)
		if err != nil {
			l.Info("skipping collaborator with non-DID repo ref", "repo", record.Repo, "err", err)
			return nil
		}
		repo, lookupErr := t.spindle.db.GetRepoByDid(repoRefDid)
		if errors.Is(lookupErr, sql.ErrNoRows) {
			t.bufferCollab(repoRefDid, evt)
			l.Info("buffering collaborator until repo arrives", "repo", repoRefDid)
			return nil
		}
		if lookupErr != nil {
			return fmt.Errorf("lookup repo %s: %w", repoRefDid, lookupErr)
		}
		repoDid := repo.RepoDid
		ownerDid := repo.Owner

		if actor != ownerDid {
			l.Info("rejecting collaborator with non-owner actor", "actor", actor, "owner", ownerDid)
			return nil
		}

		ok, err := t.spindle.e.IsCollaboratorInviteAllowed(ownerDid.String(), rbac.ThisServer, repoDid.String())
		if err != nil {
			l.Error("invite permission check failed", "err", err)
			return fmt.Errorf("invite check: %w", err)
		}
		if !ok {
			l.Info("rejecting collaborator invite", "owner", ownerDid, "repo", repoDid)
			return nil
		}

		prior, priorErr := t.spindle.db.GetRepoCollaborator(actor, rkey)
		staleSubject := priorErr == nil && (prior.Subject != subjectDid || prior.RepoDid != repoDid)

		if err := t.spindle.e.AddCollaborator(subjectDid.String(), rbac.ThisServer, repoDid.String()); err != nil {
			l.Error("failed to add collaborator policy", "err", err)
			return fmt.Errorf("add collaborator policy: %w", err)
		}
		if staleSubject {
			if err := t.spindle.e.RemoveCollaborator(prior.Subject.String(), rbac.ThisServer, prior.RepoDid.String()); err != nil {
				l.Error("failed to remove stale collaborator policy", "err", err)
				return fmt.Errorf("remove stale collaborator: %w", err)
			}
		}
		if err := t.spindle.db.AddRepoCollaborator(db.RepoCollaborator{
			OwnerDid: actor,
			Rkey:     rkey,
			Subject:  subjectDid,
			RepoDid:  repoDid,
		}); err != nil {
			l.Error("failed to persist collaborator row", "err", err)
			return fmt.Errorf("track collaborator: %w", err)
		}

	case tapc.RecordDeleteAction:
		actor := evt.Did
		rkey := evt.Rkey

		tracked, err := t.spindle.db.GetRepoCollaborator(actor, rkey)
		if err != nil {
			l.Info("skipping delete for unknown collaborator record")
			return nil
		}
		if err := t.spindle.e.RemoveCollaborator(tracked.Subject.String(), rbac.ThisServer, tracked.RepoDid.String()); err != nil {
			l.Error("failed to remove collaborator policy", "err", err)
			return fmt.Errorf("remove collaborator policy: %w", err)
		}
		if err := t.spindle.db.DeleteRepoCollaborator(actor, rkey); err != nil {
			l.Error("failed to delete collaborator row", "err", err)
			return fmt.Errorf("delete collaborator row: %w", err)
		}
	}
	return nil
}

func (t *Tap) bufferCollab(repoDid syntax.DID, evt *tapc.RecordEventData) {
	t.pendingMu.Lock()
	defer t.pendingMu.Unlock()
	list := t.pendingCollabs[repoDid]
	list = append(list, pendingCollabEvent{evt: evt, at: time.Now()})
	if len(list) > maxPendingPerRepo {
		list = list[len(list)-maxPendingPerRepo:]
	}
	t.pendingCollabs[repoDid] = list
}

func (t *Tap) drainPendingCollabs(ctx context.Context, repoDid syntax.DID) {
	t.pendingMu.Lock()
	list := t.pendingCollabs[repoDid]
	delete(t.pendingCollabs, repoDid)
	t.pendingMu.Unlock()
	if len(list) == 0 {
		return
	}
	cutoff := time.Now().Add(-pendingCollabTTL)
	for _, p := range list {
		if p.at.Before(cutoff) {
			continue
		}
		if err := t.processCollaborator(ctx, p.evt); err != nil {
			t.logger.Warn("replaying buffered collaborator failed", "repo", repoDid, "rkey", p.evt.Rkey, "err", err)
		}
	}
}

func (t *Tap) purgePendingCollabsLoop(ctx context.Context) {
	ticker := time.NewTicker(pendingCollabTTL / 2)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			t.purgeStalePendingCollabs()
		}
	}
}

func (t *Tap) purgeStalePendingCollabs() {
	cutoff := time.Now().Add(-pendingCollabTTL)
	t.pendingMu.Lock()
	defer t.pendingMu.Unlock()
	for did, list := range t.pendingCollabs {
		kept := list[:0]
		for _, p := range list {
			if !p.at.Before(cutoff) {
				kept = append(kept, p)
			}
		}
		if len(kept) == 0 {
			delete(t.pendingCollabs, did)
		} else {
			t.pendingCollabs[did] = kept
		}
	}
}
