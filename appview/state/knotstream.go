package state

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/notify"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/sites"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
	knotdb "tangled.org/core/knotserver/db"
	"tangled.org/core/log"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/workflow"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/posthog/posthog-go"
)

func Knotstream(ctx context.Context, c *config.Config, d *db.DB, enforcer *rbac.Enforcer, posthog posthog.Client, notifier notify.Notifier, cfClient *cloudflare.Client) (*ec.Consumer, error) {
	logger := log.FromContext(ctx)
	logger = log.SubLogger(logger, "knotstream")

	knots, err := db.GetRegistrations(
		d,
		orm.FilterIsNot("registered", "null"),
	)
	if err != nil {
		return nil, err
	}

	srcs := make(map[ec.Source]struct{})
	for _, k := range knots {
		s := ec.NewKnotSource(k.Domain)
		srcs[s] = struct{}{}
	}

	cache := cache.New(c.Redis.Addr)
	cursorStore := cursor.NewRedisCursorStore(cache)

	cfg := ec.ConsumerConfig{
		Sources:           srcs,
		ProcessFunc:       knotIngester(d, enforcer, posthog, notifier, c.Core.Dev, c, cfClient),
		RetryInterval:     c.Knotstream.RetryInterval,
		MaxRetryInterval:  c.Knotstream.MaxRetryInterval,
		ConnectionTimeout: c.Knotstream.ConnectionTimeout,
		WorkerCount:       c.Knotstream.WorkerCount,
		QueueSize:         c.Knotstream.QueueSize,
		Logger:            logger,
		Dev:               c.Core.Dev,
		CursorStore:       &cursorStore,
	}

	return ec.NewConsumer(cfg), nil
}

func resolveRepo(d *db.DB, repoDid *string, ownerDid, repoName string) (*models.Repo, error) {
	if repoDid != nil && *repoDid != "" {
		return db.GetRepoByDid(d, *repoDid)
	}
	repos, err := db.GetRepos(d, orm.FilterEq("did", ownerDid), orm.FilterEq("rkey", strings.ToLower(repoName)))
	if err != nil {
		return nil, err
	}
	if len(repos) == 0 {
		return nil, sql.ErrNoRows
	}
	return &repos[0], nil
}

func knotIngester(d *db.DB, enforcer *rbac.Enforcer, posthog posthog.Client, notifier notify.Notifier, dev bool, c *config.Config, cfClient *cloudflare.Client) ec.ProcessFunc {
	return func(ctx context.Context, source ec.Source, msg ec.Message) error {
		switch msg.Nsid {
		case tangled.GitRefUpdateNSID:
			return ingestRefUpdate(ctx, d, enforcer, posthog, notifier, dev, c, cfClient, source, msg)
		case tangled.PipelineNSID:
			return ingestPipeline(d, source, msg)
		case knotdb.RepoDIDAssignNSID:
			return ingestDIDAssign(d, enforcer, source, msg, ctx)
		}

		return nil
	}
}

// TODO(boltless): remove this. knotmirror should do all sort of indexing
func ingestRefUpdate(ctx context.Context, d *db.DB, enforcer *rbac.Enforcer, pc posthog.Client, notifier notify.Notifier, dev bool, c *config.Config, cfClient *cloudflare.Client, source ec.Source, msg ec.Message) error {
	logger := log.FromContext(ctx)

	var record tangled.GitRefUpdate
	err := json.Unmarshal(msg.EventJson, &record)
	if err != nil {
		return err
	}

	knownKnots, err := enforcer.GetKnotsForUser(record.CommitterDid)
	if err != nil {
		return err
	}
	if !slices.Contains(knownKnots, source.Key()) {
		return fmt.Errorf("%s does not belong to %s, something is fishy", record.CommitterDid, source.Key())
	}

	if record.Repo == "" {
		return fmt.Errorf("gitRefUpdate from %s missing repo", source.Key())
	}

	repo, lookupErr := db.GetRepoByDid(d, record.Repo)
	if lookupErr != nil {
		return fmt.Errorf("failed to look up repo: %w", lookupErr)
	}

	logger.Info("processing gitRefUpdate event",
		"repo", repo.RepoIdentifier(),
		"ref", record.Ref,
		"old_sha", record.OldSha,
		"new_sha", record.NewSha)

	notifier.Push(ctx, repo, record.Ref, record.OldSha, record.NewSha, record.CommitterDid)

	errPunchcard := populatePunchcard(d, record)
	errLanguages := updateRepoLanguages(d, record)

	var errPosthog error
	if !dev && record.CommitterDid != "" {
		errPosthog = pc.Enqueue(posthog.Capture{
			DistinctId: record.CommitterDid,
			Event:      "git_ref_update",
		})
	}

	// Trigger a sites redeploy if this push is to the configured sites branch.
	if cfClient.Enabled() {
		go triggerSitesDeployIfNeeded(ctx, d, cfClient, c, record, source)
	}

	return errors.Join(errPunchcard, errLanguages, errPosthog)
}

// triggerSitesDeployIfNeeded checks whether the pushed ref matches the sites
// branch configured for this repo and, if so, syncs the site to R2
func triggerSitesDeployIfNeeded(ctx context.Context, d *db.DB, cfClient *cloudflare.Client, cfg *config.Config, record tangled.GitRefUpdate, source ec.Source) {
	logger := log.FromContext(ctx)

	ref := plumbing.ReferenceName(record.Ref)
	if !ref.IsBranch() {
		return
	}
	pushedBranch := ref.Short()

	repo, err := db.GetRepoByDid(d, record.Repo)
	if err != nil {
		return
	}

	siteConfig, err := db.GetRepoSiteConfig(d, repo.RepoDid)
	if err != nil || siteConfig == nil {
		return
	}
	if siteConfig.Branch != pushedBranch {
		return
	}

	deploy := &models.SiteDeploy{
		RepoDid:   syntax.DID(repo.RepoDid),
		Branch:    siteConfig.Branch,
		Dir:       siteConfig.Dir,
		CommitSHA: record.NewSha,
		Trigger:   models.SiteDeployTriggerPush,
	}

	deployErr := sites.Deploy(ctx, cfClient, cfg, repo, siteConfig.Branch, siteConfig.Dir)
	if deployErr != nil {
		logger.Error("sites: R2 sync failed on push", "repo", repo.RepoIdentifier(), "err", deployErr)
		deploy.Status = models.SiteDeployStatusFailure
		deploy.Error = deployErr.Error()
	} else {
		deploy.Status = models.SiteDeployStatusSuccess
	}

	if err := db.AddSiteDeploy(d, deploy); err != nil {
		logger.Error("sites: failed to record deploy", "repo", repo.RepoIdentifier(), "err", err)
	}

	if deployErr == nil {
		logger.Info("site deployed to r2", "repo", repo.RepoIdentifier())
	}
}

func populatePunchcard(d *db.DB, record tangled.GitRefUpdate) error {
	if record.CommitterDid == "" {
		return nil
	}

	knownEmails, err := db.GetAllEmails(d, record.CommitterDid)
	if err != nil {
		return err
	}

	count := 0
	for _, ke := range knownEmails {
		if record.Meta == nil {
			continue
		}
		if record.Meta.CommitCount == nil {
			continue
		}
		for _, ce := range record.Meta.CommitCount.ByEmail {
			if ce == nil {
				continue
			}
			if ce.Email == ke.Address || ce.Email == record.CommitterDid {
				count += int(ce.Count)
			}
		}
	}

	punch := models.Punch{
		Did:   record.CommitterDid,
		Date:  time.Now(),
		Count: count,
	}
	return db.AddPunch(d, punch)
}

func updateRepoLanguages(d *db.DB, record tangled.GitRefUpdate) error {
	if record.Meta == nil || record.Meta.LangBreakdown == nil || record.Meta.LangBreakdown.Inputs == nil {
		return fmt.Errorf("empty language data for repo: %s", record.Repo)
	}

	r, lookupErr := db.GetRepoByDid(d, record.Repo)
	if lookupErr != nil {
		return fmt.Errorf("failed to look up repo: %w", lookupErr)
	}
	repo := *r

	ref := plumbing.ReferenceName(record.Ref)
	if !ref.IsBranch() {
		return fmt.Errorf("%s is not a valid reference name", ref)
	}

	var langs []models.RepoLanguage
	for _, l := range record.Meta.LangBreakdown.Inputs {
		if l == nil {
			continue
		}

		langs = append(langs, models.RepoLanguage{
			RepoDid:      syntax.DID(repo.RepoDid),
			Ref:          ref.Short(),
			IsDefaultRef: record.Meta.IsDefaultRef,
			Language:     l.Lang,
			Bytes:        l.Size,
		})
	}

	tx, err := d.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	// update appview's cache
	err = db.UpdateRepoLanguages(tx, syntax.DID(repo.RepoDid), ref.Short(), langs)
	if err != nil {
		fmt.Printf("failed; %s\n", err)
		// non-fatal
	}

	return tx.Commit()
}

func ingestPipeline(d *db.DB, source ec.Source, msg ec.Message) error {
	var record tangled.Pipeline
	err := json.Unmarshal(msg.EventJson, &record)
	if err != nil {
		return err
	}

	if record.TriggerMetadata == nil {
		return fmt.Errorf("empty trigger metadata: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	if record.TriggerMetadata.Repo == nil {
		return fmt.Errorf("empty repo: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	repoName := ""
	if record.TriggerMetadata.Repo.Repo != nil {
		repoName = *record.TriggerMetadata.Repo.Repo
	}

	repo, lookupErr := resolveRepo(d, record.TriggerMetadata.Repo.RepoDid, record.TriggerMetadata.Repo.Did, repoName)
	if lookupErr != nil {
		return fmt.Errorf("failed to look up repo: %w", lookupErr)
	}
	if repo.Spindle == "" {
		return fmt.Errorf("repo does not have a spindle configured yet: nsid %s, rkey %s", msg.Nsid, msg.Rkey)
	}

	// trigger info
	var trigger models.Trigger
	var sha string
	trigger.Kind = workflow.TriggerKind(record.TriggerMetadata.Kind)
	switch trigger.Kind {
	case workflow.TriggerKindPush:
		trigger.PushRef = &record.TriggerMetadata.Push.Ref
		trigger.PushNewSha = &record.TriggerMetadata.Push.NewSha
		trigger.PushOldSha = &record.TriggerMetadata.Push.OldSha
		sha = *trigger.PushNewSha
	case workflow.TriggerKindPullRequest:
		trigger.PRSourceBranch = &record.TriggerMetadata.PullRequest.SourceBranch
		trigger.PRTargetBranch = &record.TriggerMetadata.PullRequest.TargetBranch
		trigger.PRSourceSha = &record.TriggerMetadata.PullRequest.SourceSha
		trigger.PRAction = &record.TriggerMetadata.PullRequest.Action
		sha = *trigger.PRSourceSha
	}

	tx, err := d.Begin()
	if err != nil {
		return fmt.Errorf("failed to start txn: %w", err)
	}

	triggerId, err := db.AddTrigger(tx, trigger)
	if err != nil {
		return fmt.Errorf("failed to add trigger entry: %w", err)
	}

	pipeline := models.Pipeline{
		Rkey:      msg.Rkey,
		Knot:      source.Key(),
		RepoOwner: syntax.DID(record.TriggerMetadata.Repo.Did),
		RepoName:  repoName,
		RepoDid:   repo.RepoDid,
		TriggerId: int(triggerId),
		Sha:       sha,
	}

	err = db.AddPipeline(tx, pipeline)
	if err != nil {
		return fmt.Errorf("failed to add pipeline: %w", err)
	}

	err = tx.Commit()
	if err != nil {
		return fmt.Errorf("failed to commit txn: %w", err)
	}

	return nil
}

func ingestDIDAssign(d *db.DB, enforcer *rbac.Enforcer, source ec.Source, msg ec.Message, ctx context.Context) error {
	logger := log.FromContext(ctx)

	var record knotdb.RepoDIDAssign
	if err := json.Unmarshal(msg.EventJson, &record); err != nil {
		return fmt.Errorf("unmarshal didAssign: %w", err)
	}

	if record.RepoDid == "" || record.OwnerDid == "" || record.RepoName == "" {
		return fmt.Errorf("didAssign missing required fields: repoDid=%q ownerDid=%q repoName=%q",
			record.RepoDid, record.OwnerDid, record.RepoName)
	}

	logger.Info("processing didAssign event",
		"repo_did", record.RepoDid,
		"owner_did", record.OwnerDid,
		"repo_name", record.RepoName)

	repos, err := db.GetRepos(d,
		orm.FilterEq("did", record.OwnerDid),
		orm.FilterEq("rkey", strings.ToLower(record.RepoName)),
	)
	if err != nil || len(repos) == 0 {
		logger.Warn("didAssign for unknown repo, skipping",
			"owner_did", record.OwnerDid,
			"repo_name", record.RepoName)
		return nil
	}
	repo := repos[0]
	knot := source.Key()

	if repo.Knot != knot {
		return fmt.Errorf("didAssign from %s for repo hosted on %s, rejecting", knot, repo.Knot)
	}

	repoAtUri := repo.RepoAt().String()
	legacyResource := record.OwnerDid + "/" + record.RepoName

	if repo.RepoDid != record.RepoDid {
		tx, err := d.Begin()
		if err != nil {
			return fmt.Errorf("begin didAssign txn: %w", err)
		}
		defer tx.Rollback()

		if err := db.CascadeRepoDid(tx, repoAtUri, record.RepoDid); err != nil {
			return fmt.Errorf("cascade repo_did: %w", err)
		}

		if err := db.EnqueuePdsRewritesForRepo(tx, record.RepoDid, repoAtUri); err != nil {
			return fmt.Errorf("enqueue pds rewrites: %w", err)
		}

		if err := tx.Commit(); err != nil {
			return fmt.Errorf("commit didAssign txn: %w", err)
		}
	}

	if err := enforcer.RemoveRepo(record.OwnerDid, knot, legacyResource); err != nil {
		return fmt.Errorf("remove legacy RBAC policies for %s: %w", legacyResource, err)
	}
	if err := enforcer.AddRepo(record.OwnerDid, knot, record.RepoDid); err != nil {
		return fmt.Errorf("add RBAC policies for %s: %w", record.RepoDid, err)
	}

	collabs, collabErr := db.GetCollaborators(d, orm.FilterEq("repo_did", record.RepoDid))
	if collabErr != nil {
		return fmt.Errorf("get collaborators for RBAC update: %w", collabErr)
	}
	for _, c := range collabs {
		collabDid := c.SubjectDid.String()
		if err := enforcer.RemoveCollaborator(collabDid, knot, legacyResource); err != nil {
			return fmt.Errorf("remove collaborator RBAC for %s: %w", collabDid, err)
		}
		if err := enforcer.AddCollaborator(collabDid, knot, record.RepoDid); err != nil {
			return fmt.Errorf("add collaborator RBAC for %s: %w", collabDid, err)
		}
	}

	if err := enforcer.E.SavePolicy(); err != nil {
		return fmt.Errorf("save RBAC policies after didAssign: %w", err)
	}

	logger.Info("didAssign processed successfully",
		"repo_did", record.RepoDid,
		"owner_did", record.OwnerDid,
		"repo_name", record.RepoName)

	return nil
}
