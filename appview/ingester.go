package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"net/http"
	"net/url"
	"slices"
	"strings"
	"sync"

	"time"

	"github.com/avast/retry-go/v4"
	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/ipfs/go-cid"
	"golang.org/x/sync/errgroup"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/repoverify"
	"tangled.org/core/appview/serververify"
	"tangled.org/core/appview/validator"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
)

type Ingester struct {
	Db         *db.DB
	Enforcer   *rbac.Enforcer
	IdResolver *idresolver.Resolver
	Cache      *cache.Cache
	Config     *config.Config
	Logger     *slog.Logger
	Validator  *validator.Validator
	Notifier   notify.Notifier
	Verifier   repoverify.Verifier
}

type processFunc func(ctx context.Context, e *jmodels.Event) error

func (i *Ingester) Ingest() processFunc {
	return func(ctx context.Context, e *jmodels.Event) error {
		var err error

		l := i.Logger.With("kind", e.Kind)
		switch e.Kind {
		case jmodels.EventKindAccount:
			// TODO: sync account state to db
			if e.Account.Active {
				break
			}
			// TODO: revoke sessions by DID
			if *e.Account.Status == "deactivated" {
				err = i.IdResolver.InvalidateIdent(ctx, e.Account.Did)
			}
		case jmodels.EventKindIdentity:
			err = i.IdResolver.InvalidateIdent(ctx, e.Identity.Did)
		case jmodels.EventKindCommit:
			switch e.Commit.Collection {
			case tangled.GraphFollowNSID:
				err = i.ingestFollow(e)
			case tangled.GraphVouchNSID:
				err = i.ingestVouch(ctx, e)
			case tangled.FeedStarNSID:
				err = i.ingestStar(ctx, e)
			case tangled.PublicKeyNSID:
				err = i.ingestPublicKey(e)
			case tangled.RepoArtifactNSID:
				err = i.ingestArtifact(ctx, e)
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
			case tangled.RepoPullNSID:
				err = i.ingestPull(ctx, e)
			case tangled.RepoIssueCommentNSID:
				err = i.ingestIssueComment(e)
			case tangled.LabelDefinitionNSID:
				err = i.ingestLabelDefinition(e)
			case tangled.LabelOpNSID:
				err = i.ingestLabelOp(e)
			case tangled.RepoNSID:
				err = i.ingestRepo(ctx, e)
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

func (i *Ingester) resolveRepoRef(ref string) (*models.Repo, error) {
	if strings.HasPrefix(ref, "did:") {
		return db.GetRepoByDid(i.Db, ref)
	}
	return db.GetRepoByAtUri(i.Db, ref)
}

func (i *Ingester) resolveOldFormatStar(raw json.RawMessage, star *models.Star, l *slog.Logger) (bool, error) {
	var legacy struct {
		Subject    *string `json:"subject"`
		SubjectDid *string `json:"subjectDid"`
	}
	if err := json.Unmarshal(raw, &legacy); err != nil {
		return false, err
	}

	switch {
	case legacy.SubjectDid != nil:
		repo, err := i.resolveRepoRef(*legacy.SubjectDid)
		if err != nil {
			l.Warn("skipping old-format star for unknown repo", "subjectDid", *legacy.SubjectDid)
			return false, nil
		}
		star.SubjectType = models.StarSubjectRepo
		star.Subject = repo.RepoDid
		return true, nil

	case legacy.Subject != nil:
		uri, err := syntax.ParseATURI(*legacy.Subject)
		if err != nil {
			return false, fmt.Errorf("invalid old-format star subject: %w", err)
		}
		switch uri.Collection().String() {
		case tangled.RepoNSID:
			repo, err := db.GetRepoByAtUri(i.Db, uri.String())
			if err != nil {
				l.Warn("skipping old-format star for unknown repo", "subject", *legacy.Subject)
				return false, nil
			}
			star.SubjectType = models.StarSubjectRepo
			star.Subject = repo.RepoDid
			return true, nil
		default:
			star.SubjectType = models.StarSubjectString
			star.Subject = *legacy.Subject
			return true, nil
		}

	default:
		return false, fmt.Errorf("old-format star has neither subject nor subjectDid")
	}
}

func (i *Ingester) ingestStar(ctx context.Context, e *jmodels.Event) error {
	var err error
	did := e.Did

	l := i.Logger.With("handler", "ingestStar")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.FeedStar{}
		unmarshalErr := json.Unmarshal(raw, &record)

		star := &models.Star{
			Did:  did,
			Rkey: e.Commit.RKey,
		}

		switch {
		case unmarshalErr != nil:
			resolved, resolveErr := i.resolveOldFormatStar(raw, star, l)
			if resolveErr != nil {
				l.Error("invalid record", "newFmtErr", unmarshalErr, "oldFmtErr", resolveErr)
				return unmarshalErr
			}
			if !resolved {
				return nil
			}

		case record.Subject == nil:
			return fmt.Errorf("star record has nil subject")

		case record.Subject.FeedStar_Repo != nil:
			repo, repoErr := i.resolveRepoRef(record.Subject.FeedStar_Repo.Did)
			if repoErr != nil {
				l.Warn("skipping star for unknown repo", "did", record.Subject.FeedStar_Repo.Did)
				return nil
			}
			star.SubjectType = models.StarSubjectRepo
			star.Subject = repo.RepoDid

		case record.Subject.FeedStar_String != nil:
			star.SubjectType = models.StarSubjectString
			star.Subject = record.Subject.FeedStar_String.Uri

		default:
			return fmt.Errorf("star record has empty subject union")
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

func (i *Ingester) ingestVouch(ctx context.Context, e *jmodels.Event) error {
	var err error
	did := e.Did

	l := i.Logger.With("handler", "ingestVouch")
	l = l.With("nsid", e.Commit.Collection)
	l.Info("ingesting vouch")

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.GraphVouch{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		// rkey is the subject_did being vouched for/denounced
		subjectDID := e.Commit.RKey

		_, err = syntax.ParseDID(subjectDID)
		if err != nil {
			l.Error("invalid subject_did in rkey", "err", err, "rkey", subjectDID)
			return fmt.Errorf("invalid subject_did: %w", err)
		}

		if did == subjectDID {
			l.Warn("attempted self-vouch", "did", did)
			return fmt.Errorf("cannot vouch for self")
		}

		subjectId, err := i.IdResolver.ResolveIdent(ctx, subjectDID)
		if err != nil {
			return err
		}

		if subjectId.Handle.IsInvalidHandle() {
			return err
		}

		kind, err := models.ParseVouchKind(record.Kind)
		if err != nil {
			l.Error("invalid kind", "kind", kind)
			return fmt.Errorf("invalid kind: %s", kind)
		}

		recordCid, err := cid.Parse(e.Commit.CID)
		if err != nil {
			l.Error("invalid cid", "err", err, "cid", e.Commit.CID)
			return fmt.Errorf("invalid cid: %w", err)
		}

		var evidences []syntax.ATURI
		for _, raw := range record.Evidences {
			uri, parseErr := syntax.ParseATURI(raw)
			if parseErr != nil {
				l.Warn("invalid evidence AT-URI, skipping", "uri", raw, "err", parseErr)
				continue
			}
			evidences = append(evidences, uri)
		}

		tx, txErr := i.Db.Begin()
		if txErr != nil {
			return fmt.Errorf("failed to start transaction: %w", txErr)
		}

		addErr := db.AddVouch(tx, &models.Vouch{
			Did:        syntax.DID(did),
			SubjectDid: subjectId.DID,
			Cid:        recordCid,
			Kind:       kind,
			Reason:     record.Reason,
			Evidences:  evidences,
		})
		if addErr != nil {
			tx.Rollback()
			err = addErr
		} else {
			err = tx.Commit()
		}

	case jmodels.CommitOperationDelete:
		err = db.DeleteVouchByRkey(i.Db, did, e.Commit.RKey)
	}

	if err != nil {
		return fmt.Errorf("failed to %s vouch record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestPublicKey(e *jmodels.Event) error {
	did := e.Did
	var err error

	l := i.Logger.With("handler", "ingestPublicKey")
	l = l.With("nsid", e.Commit.Collection)

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate:
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
	case jmodels.CommitOperationUpdate:
		l.Debug("processing update of pubkey")
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.PublicKey{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		name := record.Name
		key := record.Key
		err = db.UpdatePublicKey(i.Db, did, name, key, e.Commit.RKey)
	case jmodels.CommitOperationDelete:
		l.Debug("processing delete of pubkey")
		err = db.DeletePublicKeyByRkey(i.Db, did, e.Commit.RKey)
	}

	if err != nil {
		return fmt.Errorf("failed to %s pubkey record: %w", e.Commit.Operation, err)
	}

	return nil
}

func (i *Ingester) ingestArtifact(ctx context.Context, e *jmodels.Event) error {
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
			if enqErr := db.EnqueuePdsRecordMigration(ctx, i.Db, "add-repo-did", syntax.DID(did), syntax.NSID(tangled.RepoArtifactNSID), syntax.RecordKey(e.Commit.RKey)); enqErr != nil {
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
			RepoDid:   syntax.DID(repo.RepoDid),
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

		tx, err := i.Db.Begin()
		if err != nil {
			return fmt.Errorf("failed to start transaction: %w", err)
		}

		err = db.ValidateProfile(tx, &profile)
		if err != nil {
			return fmt.Errorf("invalid profile record")
		}

		err = db.UpsertProfile(tx, &profile)
		if err == nil && i.Cache != nil {
			pipe := i.Cache.Pipeline()
			didKey := fmt.Sprintf(cache.PreferredHandleByDid, did)
			if preferredHandle != "" {
				pipe.Set(ctx, didKey, string(preferredHandle), cache.PreferredHandleTTL)
				pipe.Set(ctx, fmt.Sprintf(cache.PreferredHandleByHandle, string(preferredHandle)), did, cache.PreferredHandleTTL)
			} else {
				pipe.Del(ctx, didKey)
			}
			if _, execErr := pipe.Exec(ctx); execErr != nil {
				l.Warn("failed to update preferred handle cache", "err", execErr)
			}
		}
	case jmodels.CommitOperationDelete:
		tx, beginErr := i.Db.Begin()
		if beginErr != nil {
			return fmt.Errorf("failed to start transaction: %w", beginErr)
		}

		priorHandle, phErr := db.GetPreferredHandle(tx, did)
		if phErr != nil && !errors.Is(phErr, sql.ErrNoRows) {
			l.Warn("failed to read prior preferred handle", "err", phErr)
		}

		err = db.DeleteProfile(tx, did)
		if err == nil && i.Cache != nil {
			pipe := i.Cache.Pipeline()
			pipe.Del(ctx, fmt.Sprintf(cache.PreferredHandleByDid, did))
			if priorHandle != "" {
				pipe.Del(ctx, fmt.Sprintf(cache.PreferredHandleByHandle, string(priorHandle)))
			}
			if _, execErr := pipe.Exec(ctx); execErr != nil {
				l.Warn("failed to evict preferred handle cache", "err", execErr)
			}
		}
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

		err = db.AddSpindleMember(i.Db, models.SpindleMember{
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

		// get record from db first
		members, err := db.GetSpindleMembers(
			i.Db,
			orm.FilterEq("did", did),
			orm.FilterEq("rkey", rkey),
		)
		if err != nil || len(members) != 1 {
			return fmt.Errorf("failed to get member: %w, len(members) = %d", err, len(members))
		}
		member := members[0]

		tx, err := i.Db.Begin()
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

		err := db.AddSpindle(i.Db, models.Spindle{
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

		_, err = serververify.MarkSpindleVerified(i.Db, i.Enforcer, instance, did)
		if err != nil {
			return fmt.Errorf("failed to mark verified: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		instance := e.Commit.RKey

		// get record from db first
		spindles, err := db.GetSpindles(
			ctx,
			i.Db,
			orm.FilterEq("owner", did),
			orm.FilterEq("instance", instance),
		)
		if err != nil || len(spindles) != 1 {
			return fmt.Errorf("failed to get spindles: %w, len(spindles) = %d", err, len(spindles))
		}
		spindle := spindles[0]

		tx, err := i.Db.Begin()
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

		if err = db.AddString(i.Db, string); err != nil {
			l.Error("failed to add string", "err", err)
			return err
		}

		return nil

	case jmodels.CommitOperationDelete:
		if err := db.DeleteString(
			i.Db,
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

		err := db.AddKnot(i.Db, domain, did)
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

		err = serververify.MarkKnotVerified(i.Db, i.Enforcer, domain, did)
		if err != nil {
			return fmt.Errorf("failed to mark verified: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		domain := e.Commit.RKey

		// get record from db first
		registrations, err := db.GetRegistrations(
			i.Db,
			orm.FilterEq("domain", domain),
			orm.FilterEq("did", did),
		)
		if err != nil {
			return fmt.Errorf("failed to get registration: %w", err)
		}
		if len(registrations) != 1 {
			return fmt.Errorf("got incorrect number of registrations: %d, expected 1", len(registrations))
		}
		registration := registrations[0]

		tx, err := i.Db.Begin()
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

		if issue.RepoDid == "" {
			return fmt.Errorf("issue record has no repo field")
		}
		if _, err := syntax.ParseDID(string(issue.RepoDid)); err != nil {
			return fmt.Errorf("issue record repo field is not a valid DID: %w", err)
		}

		if err := i.Validator.ValidateIssue(&issue); err != nil {
			return fmt.Errorf("failed to validate issue: %w", err)
		}

		if record.Repo != "" && !strings.HasPrefix(record.Repo, "did:") {
			repo, repoErr := db.GetRepoByAtUri(i.Db, record.Repo)
			if repoErr == nil && repo.RepoDid != "" {
				if enqErr := db.EnqueuePdsRecordMigration(ctx, i.Db, "add-repo-did", syntax.DID(did), syntax.NSID(tangled.RepoIssueNSID), syntax.RecordKey(e.Commit.RKey)); enqErr != nil {
					l.Warn("failed to enqueue PDS rewrite for issue", "err", enqErr, "did", did, "repoDid", repo.RepoDid)
				}
			}
		}

		tx, err := i.Db.BeginTx(ctx, nil)
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
		tx, err := i.Db.BeginTx(ctx, nil)
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

func (i *Ingester) ingestPull(ctx context.Context, e *jmodels.Event) error {
	did := e.Did
	rkey := e.Commit.RKey

	var err error

	l := i.Logger.With("handler", "ingestPull", "nsid", e.Commit.Collection, "did", did, "rkey", rkey)
	l.Info("ingesting record")

	switch e.Commit.Operation {
	case jmodels.CommitOperationCreate, jmodels.CommitOperationUpdate:
		raw := json.RawMessage(e.Commit.Record)
		record := tangled.RepoPull{}
		err = json.Unmarshal(raw, &record)
		if err != nil {
			l.Error("invalid record", "err", err)
			return err
		}

		ownerId, err := i.IdResolver.ResolveIdent(ctx, did)
		if err != nil {
			l.Error("failed to resolve did")
			return err
		}

		// go through and fetch all blobs in parallel
		readers := make([]*io.ReadCloser, len(record.Rounds))
		var mu sync.Mutex

		g, gctx := errgroup.WithContext(ctx)

		for idx, b := range record.Rounds {
			g.Go(func() error {
				// for some reason, a blob is empty
				if b.PatchBlob == nil {
					return fmt.Errorf("missing patchBlob in round %d", idx)
				}

				ownerPds := ownerId.PDSEndpoint()
				url, _ := url.Parse(fmt.Sprintf("%s/xrpc/com.atproto.sync.getBlob", ownerPds))
				q := url.Query()
				q.Set("cid", b.PatchBlob.Ref.String())
				q.Set("did", did)
				url.RawQuery = q.Encode()

				req, err := http.NewRequestWithContext(gctx, http.MethodGet, url.String(), nil)
				if err != nil {
					l.Error("failed to create request")
					return err
				}
				req.Header.Set("Content-Type", "application/json")

				resp, err := http.DefaultClient.Do(req)
				if err != nil {
					l.Error("failed to make request")
					return err
				}

				mu.Lock()
				readers[idx] = &resp.Body
				mu.Unlock()

				return nil
			})
		}

		if err := g.Wait(); err != nil {
			for _, r := range readers {
				if r != nil && *r != nil {
					(*r).Close()
				}
			}
			return err
		}

		defer func() {
			for _, r := range readers {
				if r != nil && *r != nil {
					(*r).Close()
				}
			}
		}()

		pull, err := models.PullFromRecord(did, rkey, record, readers)
		if err != nil {
			return fmt.Errorf("failed to parse pull from record: %w", err)
		}
		if err := i.Validator.ValidatePull(pull); err != nil {
			return fmt.Errorf("failed to validate pull: %w", err)
		}

		tx, err := i.Db.BeginTx(ctx, nil)
		if err != nil {
			l.Error("failed to begin transaction", "err", err)
			return err
		}
		defer tx.Rollback()

		err = db.PutPull(tx, pull)
		if err != nil {
			l.Error("failed to create pull", "err", err)
			return err
		}

		err = tx.Commit()
		if err != nil {
			l.Error("failed to commit txn", "err", err)
			return err
		}

		return nil

	case jmodels.CommitOperationDelete:
		tx, err := i.Db.BeginTx(ctx, nil)
		if err != nil {
			l.Error("failed to begin transaction", "err", err)
			return err
		}
		defer tx.Rollback()

		if err := db.AbandonPulls(
			tx,
			orm.FilterEq("owner_did", did),
			orm.FilterEq("rkey", rkey),
		); err != nil {
			l.Error("failed to abandon", "err", err)
			return fmt.Errorf("failed to abandon pull record: %w", err)
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

		tx, err := i.Db.Begin()
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
			i.Db,
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

		_, err = db.AddLabelDefinition(i.Db, def)
		if err != nil {
			return fmt.Errorf("failed to create labeldef: %w", err)
		}

		return nil

	case jmodels.CommitOperationDelete:
		if err := db.DeleteLabelDefinition(
			i.Db,
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
			i, err := db.GetIssues(i.Db, orm.FilterEq("at_uri", subject))
			if err != nil || len(i) != 1 {
				return fmt.Errorf("failed to find subject: %w || subject count %d", err, len(i))
			}
			repo = i[0].Repo
		default:
			return fmt.Errorf("unsupported label subject: %s", collection)
		}

		actx, err := db.NewLabelApplicationCtx(i.Db, orm.FilterIn("at_uri", repo.Labels))
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

		tx, err := i.Db.Begin()
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
