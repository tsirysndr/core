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
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/sites"
	"tangled.org/core/consts"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventstream"
	knotdb "tangled.org/core/knotserver/db"
	"tangled.org/core/log"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/posthog/posthog-go"
)

type aclRoster interface {
	AddKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error
	RemoveKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error
	AddCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error
	RemoveCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error
	InvalidateMembers(host string)
	InvalidateCollaborators(host, repoDid string)
}

func Knotstream(ctx context.Context, c *config.Config, d *db.DB, acl *knotacl.Service, enforcer *rbac.Enforcer, posthog posthog.Client, notifier notify.Notifier, cfClient *cloudflare.Client) (*ec.Consumer, error) {
	knots, err := db.GetRegistrations(d, orm.FilterIsNot("registered", "null"))
	if err != nil {
		return nil, err
	}

	hosts := make([]string, len(knots))
	for i, k := range knots {
		hosts[i] = k.Domain
	}

	return bootstrapStream(
		ctx, "knotstream", ec.KindKnot, hosts, c.Redis.Addr,
		c.Knotstream,
		knotIngester(d, acl, enforcer, posthog, notifier, c.Core.Dev, c, cfClient),
	), nil
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

func knotIngester(d *db.DB, acl aclRoster, enforcer *rbac.Enforcer, posthog posthog.Client, notifier notify.Notifier, dev bool, c *config.Config, cfClient *cloudflare.Client) ec.ProcessFunc {
	return func(ctx context.Context, source ec.Source, msg eventstream.Event) error {
		switch msg.Nsid {
		case tangled.GitRefUpdateNSID:
			return ingestRefUpdate(ctx, d, enforcer, posthog, notifier, dev, c, cfClient, source, msg)
		case knotdb.RepoDIDAssignNSID:
			return ingestDIDAssign(d, enforcer, source, msg, ctx)
		case knotdb.KnotMemberUpdateNSID:
			return ingestKnotMemberUpdate(acl, source, msg)
		case knotdb.RepoCollaboratorUpdateNSID:
			return ingestCollaboratorUpdate(ctx, d, acl, source, msg)
		}

		return nil
	}
}

const (
	aclIngestAttempts = 3
	aclIngestBackoff  = 50 * time.Millisecond
)

func withAclRetry(attempts int, backoff time.Duration, op func() error) error {
	if err := op(); err == nil || attempts <= 1 {
		return err
	}
	time.Sleep(backoff)
	return withAclRetry(attempts-1, backoff, op)
}

func ingestKnotMemberUpdate(acl aclRoster, source ec.Source, msg eventstream.Event) error {
	var rec knotdb.KnotMemberUpdate
	if err := json.Unmarshal(msg.EventJson, &rec); err != nil {
		return fmt.Errorf("unmarshal memberUpdate: %w", err)
	}

	subject, err := syntax.ParseDID(rec.Subject)
	if err != nil {
		return fmt.Errorf("memberUpdate bad subject %q: %w", rec.Subject, err)
	}

	cursor := knotacl.Cursor(msg.Created)
	switch rec.Op {
	case knotdb.AclOpAdd:
		err = withAclRetry(aclIngestAttempts, aclIngestBackoff, func() error {
			return acl.AddKnotMember(source.Host, subject, cursor)
		})
	case knotdb.AclOpRemove:
		err = withAclRetry(aclIngestAttempts, aclIngestBackoff, func() error {
			return acl.RemoveKnotMember(source.Host, subject, cursor)
		})
	default:
		return fmt.Errorf("memberUpdate unknown op %q", rec.Op)
	}

	if err != nil {
		acl.InvalidateMembers(source.Host)
	}
	return err
}

func ingestCollaboratorUpdate(ctx context.Context, d *db.DB, acl aclRoster, source ec.Source, msg eventstream.Event) error {
	var rec knotdb.RepoCollaboratorUpdate
	if err := json.Unmarshal(msg.EventJson, &rec); err != nil {
		return fmt.Errorf("unmarshal collaboratorUpdate: %w", err)
	}

	subject, err := syntax.ParseDID(rec.Subject)
	if err != nil {
		return fmt.Errorf("collaboratorUpdate bad subject %q: %w", rec.Subject, err)
	}
	repoDid, err := syntax.ParseDID(rec.Repo)
	if err != nil {
		return fmt.Errorf("collaboratorUpdate bad repo %q: %w", rec.Repo, err)
	}

	cursor := knotacl.Cursor(msg.Created)
	switch rec.Op {
	case knotdb.AclOpAdd:
		err = withAclRetry(aclIngestAttempts, aclIngestBackoff, func() error {
			owned, err := repoOwnedBySource(ctx, d, source, repoDid, subject)
			if err != nil || !owned {
				return err
			}
			return acl.AddCollaborator(repoDid, subject, cursor)
		})
	case knotdb.AclOpRemove:
		err = withAclRetry(aclIngestAttempts, aclIngestBackoff, func() error {
			owned, err := repoOwnedBySource(ctx, d, source, repoDid, subject)
			if err != nil || !owned {
				return err
			}
			return acl.RemoveCollaborator(repoDid, subject, cursor)
		})
	default:
		return fmt.Errorf("collaboratorUpdate unknown op %q", rec.Op)
	}

	if err != nil {
		acl.InvalidateCollaborators(source.Host, repoDid.String())
	}
	return err
}

func repoOwnedBySource(ctx context.Context, d *db.DB, source ec.Source, repoDid, subject syntax.DID) (bool, error) {
	repo, err := db.GetRepoByDid(d, repoDid.String())
	if errors.Is(err, sql.ErrNoRows) {
		log.FromContext(ctx).Warn("collaboratorUpdate for unindexed repo, skipping until reconcile",
			"repo_did", repoDid, "subject", subject)
		return false, nil
	}
	if err != nil {
		return false, err
	}
	if repo.Knot != source.Host {
		log.FromContext(ctx).Warn("collaboratorUpdate for a repo this knot does not host, dropping",
			"repo_did", repoDid, "subject", subject, "claimed_by", source.Host, "owner", repo.Knot)
		return false, nil
	}
	return true, nil
}

// TODO(boltless): remove this. knotmirror should do all sort of indexing
func ingestRefUpdate(ctx context.Context, d *db.DB, enforcer *rbac.Enforcer, pc posthog.Client, notifier notify.Notifier, dev bool, c *config.Config, cfClient *cloudflare.Client, source ec.Source, msg eventstream.Event) error {
	logger := log.FromContext(ctx)

	var record tangled.GitRefUpdate
	err := json.Unmarshal(msg.EventJson, &record)
	if err != nil {
		return err
	}

	if !knotcompat.KnotHasCapability(ctx, source.Host, dev, consts.CapKnotACL) {
		knownKnots, err := enforcer.GetKnotsForUser(record.CommitterDid)
		switch {
		case err != nil:
			logger.Warn("gitRefUpdate membership lookup failed, ingesting without the sanity check", "committer", record.CommitterDid, "knot", source.Host, "err", err)
		case !slices.Contains(knownKnots, source.Host):
			logger.Warn("gitRefUpdate committer is not a known member of the knot, ingesting anyway", "committer", record.CommitterDid, "knot", source.Host)
		}
	}

	if record.Repo == "" {
		return fmt.Errorf("gitRefUpdate from %s missing repo", source.Host)
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

func ingestDIDAssign(d *db.DB, enforcer *rbac.Enforcer, source ec.Source, msg eventstream.Event, ctx context.Context) error {
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
		orm.FilterEq("name", record.RepoName),
	)
	if err != nil || len(repos) == 0 {
		logger.Warn("didAssign for unknown repo, skipping",
			"owner_did", record.OwnerDid,
			"repo_name", record.RepoName)
		return nil
	}
	repo := repos[0]
	knot := source.Host

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
