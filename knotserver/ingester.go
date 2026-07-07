package knotserver

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/db"
	knotxrpc "tangled.org/core/knotserver/xrpc"
	"tangled.org/core/log"
)

func (h *Knot) processPublicKey(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx).With("handler", "processPublicKey", "did", event.Did, "rkey", event.Commit.RKey)
	did := syntax.DID(event.Did)
	rkey := syntax.RecordKey(event.Commit.RKey)

	switch event.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		var record tangled.PublicKey
		if err := json.Unmarshal(json.RawMessage(event.Commit.Record), &record); err != nil {
			return fmt.Errorf("failed to unmarshal record: %w", err)
		}

		pk := db.PublicKey{
			Did:       did,
			Rkey:      rkey,
			PublicKey: record,
		}
		if err := h.db.UpsertPublicKey(pk); err != nil {
			return fmt.Errorf("failed to upsert public key: %w", err)
		}
		l.Info("upserted public key from firehose")
	case jmodels.CommitOperationDelete:
		if err := h.db.DeletePublicKeyByRkey(did, rkey); err != nil {
			return fmt.Errorf("failed to delete public key: %w", err)
		}
		l.Info("deleted public key from firehose")
	}

	return nil
}

// returns a repo path on disk if present, and error if not
type targetRepo struct {
	RepoPath      string
	OwnerDid      string
	RepoName      string
	RepoDid       string
	DefaultBranch string // default branch
}

func (h *Knot) processRepo(ctx context.Context, event *jmodels.Event) error {
	l := log.FromContext(ctx).With("handler", "processRepo", "did", event.Did, "rkey", event.Commit.RKey)

	rkey := strings.TrimSuffix(strings.TrimSpace(event.Commit.RKey), ".git")
	if rkey == "" {
		return nil
	}

	if event.Commit.Operation == jmodels.CommitOperationDelete {
		return nil
	}

	if event.Commit.Operation != jmodels.CommitOperationCreate && event.Commit.Operation != jmodels.CommitOperationUpdate {
		return nil
	}

	raw := json.RawMessage(event.Commit.Record)
	var record tangled.Repo
	if err := json.Unmarshal(raw, &record); err != nil {
		return fmt.Errorf("failed to unmarshal repo record: %w", err)
	}

	if record.Knot != h.c.Server.Hostname {
		return nil
	}
	if record.RepoDid == nil || *record.RepoDid == "" {
		l.Info("skipping repo event without repoDid")
		return nil
	}
	repoDid := *record.RepoDid

	if err := knotxrpc.ValidateRepoName(rkey); err != nil {
		l.Warn("skipping repo event with invalid rkey", "repoDid", repoDid, "rkey", rkey, "err", err)
		return nil
	}

	ownerDid, _, lookupErr := h.db.GetRepoKeyOwner(repoDid)
	if lookupErr != nil {
		l.Info("skipping repo event for unknown repoDid", "repoDid", repoDid)
		return nil
	}
	if ownerDid != event.Did {
		l.Warn("repo event author does not own repoDid", "repoDid", repoDid, "author", event.Did)
		return nil
	}

	alias := db.RepoAlias{
		OwnerDid: event.Did,
		Rkey:     rkey,
		RepoDid:  repoDid,
		Rev:      event.Commit.Rev,
	}
	if err := h.db.UpsertRepoAlias(alias); err != nil {
		l.Warn("failed to upsert repo alias", "err", err)
		return nil
	}

	l.Info("recorded repo alias", "repoDid", repoDid, "rkey", rkey, "rev", event.Commit.Rev)
	return nil
}

func (h *Knot) processMessages(ctx context.Context, event *jmodels.Event) error {
	var err error
	switch event.Kind {
	case jmodels.EventKindIdentity:
		err = h.resolver.InvalidateIdent(ctx, event.Did)
	case jmodels.EventKindCommit:
		switch event.Commit.Collection {
		case tangled.PublicKeyNSID:
			err = h.processPublicKey(ctx, event)
		case tangled.RepoNSID:
			err = h.processRepo(ctx, event)
		}
	default:
		return nil
	}

	if err != nil {
		args := []any{"kind", event.Kind, "err", err}
		if event.Kind == jmodels.EventKindCommit {
			args = append(args, "nsid", event.Commit.Collection, "did", event.Did, "rkey", event.Commit.RKey)
		}
		h.l.Warn("failed to process event, skipping", args...)
	}

	return nil
}
