package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"slices"

	"time"

	"github.com/avast/retry-go/v4"
	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/ipfs/go-cid"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/serververify"
	"tangled.org/core/appview/validator"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
)

type Ingester struct {
	Db         db.DbWrapper
	Enforcer   *rbac.Enforcer
	IdResolver *idresolver.Resolver
	Config     *config.Config
	Logger     *slog.Logger
	Validator  *validator.Validator
}

type processFunc func(ctx context.Context, e *jmodels.Event) error

func (i *Ingester) Ingest() processFunc {
	return func(ctx context.Context, e *jmodels.Event) error {
		var err error

		l := i.Logger.With("kind", e.Kind)
		switch e.Kind {
		case jmodels.EventKindAccount:
			if !e.Account.Active && *e.Account.Status == "deactivated" {
				err = i.IdResolver.InvalidateIdent(ctx, e.Account.Did)
			}
		case jmodels.EventKindIdentity:
			err = i.IdResolver.InvalidateIdent(ctx, e.Identity.Did)
		case jmodels.EventKindCommit:
			switch e.Commit.Collection {
			case tangled.GraphFollowNSID:
				err = i.ingestFollow(e)
			case tangled.FeedStarNSID:
				err = i.ingestStar(e)
			case tangled.PublicKeyNSID:
				err = i.ingestPublicKey(e)
			case tangled.RepoArtifactNSID:
				err = i.ingestArtifact(e)
			case tangled.ActorProfileNSID:
				err = i.ingestProfile(ctx, e)
			case tangled.SpindleMemberNSID:
				err = i.ingestSpindleMember(ctx, e)
			case tangled.SpindleNSID:
				err = i.ingestSpindle(ctx, e)
			case tangled.KnotMemberNSID:
				err = i.ingestKnotMember(e)
			case tangled.KnotNSID:
				err = i.ingestKnot(e)
			case tangled.StringNSID:
				err = i.ingestString(e)
			case tangled.RepoIssueNSID:
				err = i.ingestIssue(ctx, e)
			case tangled.RepoIssueCommentNSID:
				err = i.ingestIssueComment(e)
			case tangled.LabelDefinitionNSID:
				err = i.ingestLabelDefinition(e)
			case tangled.LabelOpNSID:
				err = i.ingestLabelOp(e)
			}
			l = i.Logger.With("nsid", e.Commit.Collection)
		}

		if err != nil {
			l.Warn("failed to ingest record, skipping", "err", err)
		}

		lastTimeUs := e.TimeUS + 1
		if saveErr := i.Db.SaveLastTimeUs(lastTimeUs); saveErr != nil {
			l.Error("failed to save cursor", "err", saveErr)
		}

		return nil
	}
}

func (i *Ingester) ingestStar(e *jmodels.Event) error {
	var err error
	did := e.Did

	l := i.Logger.With("handler", "ingestStar")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		var subjectUri syntax.ATURI

		raw := json.RawMessage(e.Commit.Record)
		record := tangled.FeedStar{}
		err := json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		star := &models.Star{
			Did:  did,
			Rkey: e.Commit.RKey,
		}

		switch {
		case record.SubjectDid != nil:
			repo, repoErr := db.GetRepo(i.Db, orm.FilterEq("repo_did", *record.SubjectDid))
			if repoErr == nil {
				subjectUri = repo.RepoAt()
				star.RepoAt = subjectUri
			}
		case record.Subject != nil:
			subjectUri, err = syntax.ParseATURI(*record.Subject)
			if err != nil {
				l.Error("invalid record", "err", err)
				return err
			}
			star.RepoAt = subjectUri
			repo, repoErr := db.GetRepoByAtUri(i.Db, subjectUri.String())
			if repoErr == nil && repo.RepoDid != "" {
				if enqErr := db.EnqueuePdsRewrite(i.Db, did, repo.RepoDid, tangled.FeedStarNSID, e.Commit.RKey, *record.Subject); enqErr != nil {
					l.Warn("failed to enqueue PDS rewrite for star", "err", enqErr, "did", did, "repoDid", repo.RepoDid)
				}
			}
		default:
			l.Error("star record has neither subject nor subjectDid")
			return fmt.Errorf("star record has neither subject nor subjectDid")
		}
		err = db.AddStar(i.Db, star)
	case jmodels.CommitOperationDelete:
		err = db.DeleteStarByRkey(i.Db, did, e.Commit.RKey)
	}

	if err != nil {
		return fmt.Errorf("failed to %s star record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestFollow(e *jmodels.Event) error {
	var err error
	did := e.Did

	l := i.Logger.With("handler", "ingestFollow")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.GraphFollow{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		err = db.AddFollow(i.Db, &models.Follow{
			UserDid:    did,
			SubjectDid: record.Subject,
			Rkey:       e.Commit.RKey,
		})
	case jmodels.CommitOperationDelete:
		err = db.DeleteFollowByRkey(i.Db, did, e.Commit.RKey)
	}

	if err != nil {
		return fmt.Errorf("failed to %s follow record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestPublicKey(e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestPublicKey")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		l.Debug("processing add of pubkey")
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.PublicKey{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		name := record.Name
		key := record.Key
		err = db.AddPublicKey(i.Db, did, name, key, e.Commit.RKey)
	case jmodels.CommitOperationDelete:
		l.Debug("processing delete of pubkey")
		err = db.DeletePublicKeyByRkey(i.Db, did, e.Commit.RKey)
	}

	if err != nil {
		return fmt.Errorf("failed to %s pubkey record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestArtifact(e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestArtifact")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.RepoArtifact{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		var repo *models.Repo
		if record.RepoDid != nil && *record.RepoDid != "" {
			repo, err = db.GetRepoByDid(i.Db, *record.RepoDid)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return fmt.Errorf("failed to look up repo by DID %s: %w", *record.RepoDid, err)
			}
		}
		if repo == nil && record.Repo != nil {
			repoAt, parseErr := syntax.ParseATURI(*record.Repo)
			if parseErr != nil {
				return parseErr
			}
			repo, err = db.GetRepoByAtUri(i.Db, repoAt.String())
			if err != nil {
				return err
			}
		}
		if repo == nil {
			return fmt.Errorf("artifact record has neither valid repoDid nor repo field")
		}

		ok, err := i.Enforcer.E.Enforce(did, repo.Knot, repo.RepoIdentifier(), "repo:push")
		if err != nil || !ok {
			return err
		}

		repoDid := repo.RepoDid
		if repoDid == "" && record.RepoDid != nil {
			repoDid = *record.RepoDid
		}
		if repoDid != "" && (record.RepoDid == nil || *record.RepoDid == "") && record.Repo != nil {
			if enqErr := db.EnqueuePdsRewrite(i.Db, did, repoDid, tangled.RepoArtifactNSID, e.Commit.RKey, *record.Repo); enqErr != nil {
				l.Warn("failed to enqueue PDS rewrite for artifact", "err", enqErr, "did", did, "repoDid", repoDid)
			}
		}

		createdAt, err := time.Parse(time.RFC3339, record.CreatedAt)
		if err != nil {
			createdAt = time.Now()
		}

		artifact := models.Artifact{
			Did:       did,
			Rkey:      e.Commit.RKey,
			RepoAt:    repo.RepoAt(),
			Tag:       plumbing.Hash(record.Tag),
			CreatedAt: createdAt,
			BlobCid:   cid.Cid(record.Artifact.Ref),
			Name:      record.Name,
			Size:      uint64(record.Artifact.Size),
			MimeType:  record.Artifact.MimeType,
		}

		err = db.AddArtifact(i.Db, artifact)
	case jmodels.CommitOperationDelete:
		err = db.DeleteArtifact(i.Db, orm.FilterEq("did", did), orm.FilterEq("rkey", e.Commit.RKey))
	}

	if err != nil {
		return fmt.Errorf("failed to %s artifact record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestProfile(ctx context.Context, e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestProfile")
	l = l.With("nsid", e.Commit.Collection)

	if e.Commit.RKey != "self" {
		return fmt.Errorf("ingestProfile only ingests `self` record")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.ActorProfile{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		avatar := ""
		if record.Avatar != nil {
			avatar = record.Avatar.Ref.String()
		}

		description := ""
		if record.Description != nil {
			description = *record.Description
		}

		includeBluesky := record.Bluesky

		pronouns := ""
		if record.Pronouns != nil {
			pronouns = *record.Pronouns
		}

		location := ""
		if record.Location != nil {
			location = *record.Location
		}

		var links [5]string
		for i, l := range record.Links {
			if i < 5 {
				links[i] = l
			}
		}

		var stats [2]models.VanityStat
		for i, s := range record.Stats {
			if i < 2 {
				stats[i].Kind = models.ParseVanityStatKind(s)
			}
		}

		var pinned [6]string
		for i, r := range record.PinnedRepositories {
			if i < 6 {
				pinned[i] = r
			}
		}

		var preferredHandle syntax.Handle
		if record.PreferredHandle != nil {
			if h, err := syntax.ParseHandle(*record.PreferredHandle); err == nil {
				ident, identErr := i.IdResolver.ResolveIdent(ctx, did)
				if identErr == nil && slices.Contains(ident.AlsoKnownAs, "at://"+string(h)) {
					preferredHandle = h
				}
			}
		}

		profile := models.Profile{
			Did:             did,
			Avatar:          avatar,
			Description:     description,
			IncludeBluesky:  includeBluesky,
			Location:        location,
			Links:           links,
			Stats:           stats,
			PinnedRepos:     pinned,
			Pronouns:        pronouns,
			PreferredHandle: preferredHandle,
		}

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index profile record, invalid db cast")
		}

		tx, err := ddb.Begin()
		if err != nil {
			return fmt.Errorf("failed to start transaction")
		}

		err = db.ValidateProfile(tx, &profile)
		if err != nil {
			return fmt.Errorf("invalid profile record")
		}

		err = db.UpsertProfile(tx, &profile)
	case jmodels.CommitOperationDelete:
		err = db.DeleteArtifact(i.Db, orm.FilterEq("did", did), orm.FilterEq("rkey", e.Commit.RKey))
	}

	if err != nil {
		return fmt.Errorf("failed to %s profile record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestSpindleMember(ctx context.Context, e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestSpindleMember")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.SpindleMember{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		// only spindle owner can invite to spindles
		ok, err := i.Enforcer.IsSpindleInviteAllowed(did, record.Instance)
		if err != nil || !ok {
			return fmt.Errorf("failed to enforce permissions: %w", err)
		}

		memberId, err := i.IdResolver.ResolveIdent(ctx, record.Subject)
		if err != nil {
			return err
		}

		if memberId.Handle.IsInvalidHandle() {
			return err
		}

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("invalid db cast")
		}

		err = db.AddSpindleMember(ddb, models.SpindleMember{
			Did:      syntax.DID(did),
			Rkey:     e.Commit.RKey,
			Instance: record.Instance,
			Subject:  memberId.DID,
		})
		if !ok {
			return fmt.Errorf("failed to add to db: %w", err)
		}

		err = i.Enforcer.AddSpindleMember(record.Instance, memberId.DID.String())
		if err != nil {
			return fmt.Errorf("failed to update ACLs: %w", err)
		}

		l.Info("added spindle member")
	case jmodels.CommitOperationDelete:
		rkey := e.Commit.RKey

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index profile record, invalid db cast")
		}

		// get record from db first
		members, err := db.GetSpindleMembers(
			ddb,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		)
		if err != nil || len(members) != 1 {
			return fmt.Errorf("failed to get member: %w, len(members) = %d", err, len(members))
		}
		member := members[0]

		tx, err := ddb.Begin()
		if err != nil {
			return fmt.Errorf("failed to start txn: %w", err)
		}

		// remove record by rkey && update enforcer
		if err = db.RemoveSpindleMember(
			tx,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		); err != nil {
			return fmt.Errorf("failed to remove from db: %w", err)
		}

		// update enforcer
		err = i.Enforcer.RemoveSpindleMember(member.Instance, member.Subject.String())
		if err != nil {
			return fmt.Errorf("failed to update ACLs: %w", err)
		}

		if err = tx.Commit(); err != nil {
			return fmt.Errorf("failed to commit txn: %w", err)
		}

		if err = i.Enforcer.E.SavePolicy(); err != nil {
			return fmt.Errorf("failed to save ACLs: %w", err)
		}

		l.Info("removed spindle member")
	}

	return nil
}

func (i *Ingester) ingestSpindle(ctx context.Context, e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestSpindle")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.Spindle{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		instance := e.Commit.RKey

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index profile record, invalid db cast")
		}

		err := db.AddSpindle(ddb, models.Spindle{
			Owner:    syntax.DID(did),
			Instance: instance,
		})
		if err != nil {
			l.Error("failed to add spindle to db", "err", err, "instance", instance)
			return err
		}

		err = retry.Do(
			func() error { return serververify.RunVerification(ctx, instance, did, i.Config.Core.Dev) },
			retry.Attempts(5), retry.Delay(5*time.Second), retry.MaxDelay(80*time.Second),
			retry.DelayType(retry.BackOffDelay), retry.LastErrorOnly(true),
		)
		if err != nil {
			l.Error("failed to verify spindle after retries", "err", err, "instance", instance)
			return err
		}

		_, err = serververify.MarkSpindleVerified(ddb, i.Enforcer, instance, did)
		if err != nil {
			return fmt.Errorf("failed to mark verified: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		instance := e.Commit.RKey

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index profile record, invalid db cast")
		}

		// get record from db first
		spindles, err := db.GetSpindles(
			ctx,
			ddb,
			orm.FilterEq("owner", did),
			orm.FilterEq("instance", instance),
		)
		if err != nil || len(spindles) != 1 {
			return fmt.Errorf("failed to get spindles: %w, len(spindles) = %d", err, len(spindles))
		}
		spindle := spindles[0]

		tx, err := ddb.Begin()
		if err != nil {
			return err
		}
		defer func() {
			tx.Rollback()
			i.Enforcer.E.LoadPolicy()
		}()

		// remove spindle members first
		err = db.RemoveSpindleMember(
			tx,
			orm.FilterEq("owner", did),
			orm.FilterEq("instance", instance),
		)
		if err != nil {
			return err
		}

		err = db.DeleteSpindle(
			tx,
			orm.FilterEq("owner", did),
			orm.FilterEq("instance", instance),
		)
		if err != nil {
			return err
		}

		if spindle.Verified != nil {
			err = i.Enforcer.RemoveSpindle(instance)
			if err != nil {
				return err
			}
		}

		err = tx.Commit()
		if err != nil {
			return err
		}

		err = i.Enforcer.E.SavePolicy()
		if err != nil {
			return err
		}
	}

	return nil
}

func (i *Ingester) ingestString(e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestString", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	ddb, ok := i.Db.Execer.(*db.DB)
	if !ok {
		return fmt.Errorf("failed to index string record, invalid db cast")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.String{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		string := models.StringFromRecord(did, rkey, record)

		if err = i.Validator.ValidateString(&string); err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		if err = db.AddString(ddb, string); err != nil {
			l.Error("failed to add string", "err", err)
			return err
		}

		return nil

	case jmodels.CommitOperationDelete:
		if err := db.DeleteString(
			ddb,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		); err != nil {
			l.Error("failed to delete", "err", err)
			return fmt.Errorf("failed to delete string record: %w", err)
		}

		return nil
	}

	return nil
}

func (i *Ingester) ingestKnotMember(e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestKnotMember")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.KnotMember{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		// only knot owner can invite to knots
		ok, err := i.Enforcer.IsKnotInviteAllowed(did, record.Domain)
		if err != nil || !ok {
			return fmt.Errorf("failed to enforce permissions: %w", err)
		}

		memberId, err := i.IdResolver.ResolveIdent(context.Background(), record.Subject)
		if err != nil {
			return err
		}

		if memberId.Handle.IsInvalidHandle() {
			return err
		}

		err = i.Enforcer.AddKnotMember(record.Domain, memberId.DID.String())
		if err != nil {
			return fmt.Errorf("failed to update ACLs: %w", err)
		}

		l.Info("added knot member")
	case jmodels.CommitOperationDelete:
		// we don't store knot members in a table (like we do for spindle)
		// and we can't remove this just yet. possibly fixed if we switch
		// to either:
		//   1. a knot_members table like with spindle and store the rkey
		// 	 2. use the knot host as the rkey
		//
		// TODO: implement member deletion
		l.Info("skipping knot member delete", "did", did, "rkey", e.Commit.RKey)
	}

	return nil
}

func (i *Ingester) ingestKnot(e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestKnot")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.Knot{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		domain := e.Commit.RKey

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index profile record, invalid db cast")
		}

		err := db.AddKnot(ddb, domain, did)
		if err != nil {
			l.Error("failed to add knot to db", "err", err, "domain", domain)
			return err
		}

		err = retry.Do(
			func() error {
				return serververify.RunVerification(context.Background(), domain, did, i.Config.Core.Dev)
			},
			retry.Attempts(5), retry.Delay(5*time.Second), retry.MaxDelay(80*time.Second),
			retry.DelayType(retry.BackOffDelay), retry.LastErrorOnly(true),
		)
		if err != nil {
			l.Error("failed to verify knot after retries", "err", err, "domain", domain)
			return err
		}

		err = serververify.MarkKnotVerified(ddb, i.Enforcer, domain, did)
		if err != nil {
			return fmt.Errorf("failed to mark verified: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		domain := e.Commit.RKey

		ddb, ok := i.Db.Execer.(*db.DB)
		if !ok {
			return fmt.Errorf("failed to index knot record, invalid db cast")
		}

		// get record from db first
		registrations, err := db.GetRegistrations(
			ddb,
			orm.FilterEq("domain", domain),
			orm.FilterEq("did", did),
		)
		if err != nil {
			return fmt.Errorf("failed to get registration: %w", err)
		}
		if len(registrations) != 1 {
			return fmt.Errorf("got incorret number of registrations: %d, expected 1", len(registrations))
		}
		registration := registrations[0]

		tx, err := ddb.Begin()
		if err != nil {
			return err
		}
		defer func() {
			tx.Rollback()
			i.Enforcer.E.LoadPolicy()
		}()

		err = db.DeleteKnot(
			tx,
			orm.FilterEq("did", did),
			orm.FilterEq("domain", domain),
		)
		if err != nil {
			return err
		}

		if registration.Registered != nil {
			err = i.Enforcer.RemoveKnot(domain)
			if err != nil {
				return err
			}
		}

		err = tx.Commit()
		if err != nil {
			return err
		}

		err = i.Enforcer.E.SavePolicy()
		if err != nil {
			return err
		}
	}

	return nil
}
func (i *Ingester) ingestIssue(ctx context.Context, e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestIssue", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	ddb, ok := i.Db.Execer.(*db.DB)
	if !ok {
		return fmt.Errorf("failed to index issue record, invalid db cast")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.RepoIssue{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		issue := models.IssueFromRecord(did, rkey, record)

		if issue.RepoAt == "" {
			return fmt.Errorf("issue record has no repo field")
		}

		if err := i.Validator.ValidateIssue(&issue); err != nil {
			return fmt.Errorf("failed to validate issue: %w", err)
		}

		if record.Repo != nil {
			repo, repoErr := db.GetRepoByAtUri(i.Db, *record.Repo)
			if repoErr == nil && repo.RepoDid != "" {
				if enqErr := db.EnqueuePdsRewrite(i.Db, did, repo.RepoDid, tangled.RepoIssueNSID, rkey, *record.Repo); enqErr != nil {
					l.Warn("failed to enqueue PDS rewrite for issue", "err", enqErr, "did", did, "repoDid", repo.RepoDid)
				}
			}
		}

		tx, err := ddb.BeginTx(ctx, nil)
		if err != nil {
			l.Error("failed to begin transaction", "err", err)
			return err
		}
		defer tx.Rollback()

		err = db.PutIssue(tx, &issue)
		if err != nil {
			l.Error("failed to create issue", "err", err)
			return err
		}

		err = tx.Commit()
		if err != nil {
			l.Error("failed to commit txn", "err", err)
			return err
		}

		return nil

	case jmodels.CommitOperationDelete:
		tx, err := ddb.BeginTx(ctx, nil)
		if err != nil {
			l.Error("failed to begin transaction", "err", err)
			return err
		}
		defer tx.Rollback()

		if err := db.DeleteIssues(
			tx,
			did,
			rkey,
		); err != nil {
			l.Error("failed to delete", "err", err)
			return fmt.Errorf("failed to delete issue record: %w", err)
		}
		if err := tx.Commit(); err != nil {
			l.Error("failed to commit txn", "err", err)
			return err
		}

		return nil
	}

	return nil
}

func (i *Ingester) ingestIssueComment(e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestIssueComment", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	ddb, ok := i.Db.Execer.(*db.DB)
	if !ok {
		return fmt.Errorf("failed to index issue comment record, invalid db cast")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.RepoIssueComment{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			return fmt.Errorf("invalid record: %w", err)
		}

		comment, err := models.IssueCommentFromRecord(did, rkey, record)
		if err != nil {
			return fmt.Errorf("failed to parse comment from record: %w", err)
		}

		if err := i.Validator.ValidateIssueComment(comment); err != nil {
			return fmt.Errorf("failed to validate comment: %w", err)
		}

		tx, err := ddb.Begin()
		if err != nil {
			return fmt.Errorf("failed to start transaction: %w", err)
		}
		defer tx.Rollback()

		_, err = db.AddIssueComment(tx, *comment)
		if err != nil {
			return fmt.Errorf("failed to create issue comment: %w", err)
		}

		return tx.Commit()

	case jmodels.CommitOperationDelete:
		if err := db.DeleteIssueComments(
			ddb,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		); err != nil {
			return fmt.Errorf("failed to delete issue comment record: %w", err)
		}

		return nil
	}

	return nil
}

func (i *Ingester) ingestLabelDefinition(e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestLabelDefinition", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	ddb, ok := i.Db.Execer.(*db.DB)
	if !ok {
		return fmt.Errorf("failed to index label definition, invalid db cast")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.LabelDefinition{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			return fmt.Errorf("invalid record: %w", err)
		}

		def, err := models.LabelDefinitionFromRecord(did, rkey, record)
		if err != nil {
			return fmt.Errorf("failed to parse labeldef from record: %w", err)
		}

		if err := i.Validator.ValidateLabelDefinition(def); err != nil {
			return fmt.Errorf("failed to validate labeldef: %w", err)
		}

		_, err = db.AddLabelDefinition(ddb, def)
		if err != nil {
			return fmt.Errorf("failed to create labeldef: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		if err := db.DeleteLabelDefinition(
			ddb,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		); err != nil {
			return fmt.Errorf("failed to delete labeldef record: %w", err)
		}

		return nil
	}

	return nil
}

func (i *Ingester) ingestLabelOp(e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestLabelOp", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	ddb, ok := i.Db.Execer.(*db.DB)
	if !ok {
		return fmt.Errorf("failed to index label op, invalid db cast")
	}

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.LabelOp{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			return fmt.Errorf("invalid record: %w", err)
		}

		subject := syntax.ATURI(record.Subject)
		collection := subject.Collection()

		var repo *models.Repo
		switch collection {
		case tangled.RepoIssueNSID:
			i, err := db.GetIssues(ddb, orm.FilterEq("at_uri", subject))
			if err != nil || len(i) != 1 {
				return fmt.Errorf("failed to find subject: %w || subject count %d", err, len(i))
			}
			repo = i[0].Repo
		default:
			return fmt.Errorf("unsupport label subject: %s", collection)
		}

		actx, err := db.NewLabelApplicationCtx(ddb, orm.FilterIn("at_uri", repo.Labels))
		if err != nil {
			return fmt.Errorf("failed to build label application ctx: %w", err)
		}

		ops := models.LabelOpsFromRecord(did, rkey, record)

		for _, o := range ops {
			def, ok := actx.Defs[o.OperandKey]
			if !ok {
				return fmt.Errorf("failed to find label def for key: %s, expected: %q", o.OperandKey, slices.Collect(maps.Keys(actx.Defs)))
			}
			if err := i.Validator.ValidateLabelOp(def, repo, &o); err != nil {
				return fmt.Errorf("failed to validate labelop: %w", err)
			}
		}

		tx, err := ddb.Begin()
		if err != nil {
			return err
		}
		defer tx.Rollback()

		for _, o := range ops {
			_, err = db.AddLabelOp(tx, &o)
			if err != nil {
				return fmt.Errorf("failed to add labelop: %w", err)
			}
		}

		if err = tx.Commit(); err != nil {
			return err
		}
	}

	return nil
}
