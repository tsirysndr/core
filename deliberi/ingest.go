package deliberi

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	deldb "tangled.org/core/deliberi/db"
	models "tangled.org/core/deliberi/models"
	js "tangled.org/core/jetstream"
)

type Ingester struct {
	db         *deldb.DB
	recipients recipientResolver
	jc         *js.JetstreamClient
	logger     *slog.Logger
}

var ingestCollections = []string{
	tangled.RepoNSID,
	tangled.RepoIssueNSID,
	tangled.RepoPullNSID,
	tangled.FeedCommentNSID,
	tangled.FeedStarNSID,
	tangled.GraphFollowNSID,
}

func NewIngester(database *deldb.DB, recipients recipientResolver, endpoint, ident string, logger *slog.Logger) (*Ingester, error) {
	jc, err := js.NewJetstreamClient(endpoint, ident, ingestCollections, nil, logger, database, false, false)
	if err != nil {
		return nil, fmt.Errorf("creating jetstream client: %w", err)
	}
	return &Ingester{
		db:         database,
		recipients: recipients,
		jc:         jc,
		logger:     logger,
	}, nil
}

func (i *Ingester) Run(ctx context.Context) error {
	return i.jc.StartJetstream(ctx, i.process)
}

func (i *Ingester) process(ctx context.Context, e *jmodels.Event) error {
	if e.Kind != jmodels.EventKindCommit || e.Commit == nil {
		return nil
	}
	if e.Commit.Operation != jmodels.CommitOperationCreate && e.Commit.Operation != jmodels.CommitOperationUpdate {
		return nil
	}

	actorDid := e.Did
	entityAt := fmt.Sprintf("at://%s/%s/%s", e.Did, e.Commit.Collection, e.Commit.RKey)

	switch e.Commit.Collection {
	case tangled.RepoNSID:
		var rec tangled.Repo
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding repo record", "err", err, "uri", entityAt)
			return nil
		}
		ownerDid := e.Did
		repoDid := ownerDid
		name := ""
		if rec.Name != nil {
			name = *rec.Name
		}
		if rec.RepoDid != nil && *rec.RepoDid != "" {
			repoDid = *rec.RepoDid
		}
		if err := deldb.PutRepoName(i.db, repoDid, ownerDid, name); err != nil {
			i.logger.Warn("caching repo name", "err", err, "repoDid", repoDid, "ownerDid", ownerDid)
		}

	case tangled.RepoIssueNSID:
		var rec tangled.RepoIssue
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding issue record", "err", err, "uri", entityAt)
			return nil
		}
		if err := deldb.PutEntityTitle(i.db, entityAt, rec.Title); err != nil {
			i.logger.Warn("caching entity title", "err", err, "uri", entityAt)
		}
		i.notifyEntity(ctx, actorDid, entityAt, entityAt, rec.Repo, models.NotificationTypeIssueCreated, rec.Title, rec.Mentions)

	case tangled.RepoPullNSID:
		var rec tangled.RepoPull
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding pull record", "err", err, "uri", entityAt)
			return nil
		}
		repoDid := ""
		if rec.Target != nil {
			repoDid = rec.Target.Repo
		}
		if err := deldb.PutEntityTitle(i.db, entityAt, rec.Title); err != nil {
			i.logger.Warn("caching entity title", "err", err, "uri", entityAt)
		}
		i.notifyEntity(ctx, actorDid, entityAt, entityAt, repoDid, models.NotificationTypePullCreated, rec.Title, rec.Mentions)

	case tangled.FeedCommentNSID:
		var rec tangled.FeedComment
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding comment record", "err", err, "uri", entityAt)
			return nil
		}
		if rec.Subject == nil {
			return nil
		}
		subjectUri := rec.Subject.Uri
		var t models.NotificationType
		switch syntax.ATURI(subjectUri).Collection().String() {
		case tangled.RepoIssueNSID:
			t = models.NotificationTypeIssueCommented
		case tangled.RepoPullNSID:
			t = models.NotificationTypePullCommented
		default:
			return nil
		}
		// comment carries no repo did and no mentions field; leave both empty.
		title := deldb.GetEntityTitle(i.db, subjectUri)
		i.notifyEntity(ctx, actorDid, entityAt, subjectUri, "", t, title, nil)

	case tangled.FeedStarNSID:
		var rec tangled.FeedStar
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding star record", "err", err, "uri", entityAt)
			return nil
		}
		if rec.Subject == nil || rec.Subject.FeedStar_Repo == nil {
			return nil
		}
		repoDid := rec.Subject.FeedStar_Repo.Did
		recipientDid := i.hydrateRepoOwner(ctx, repoDid)
		if recipientDid == "" {
			i.logger.Warn("star: could not resolve repo owner, skipping notification", "repoDid", repoDid)
			return nil
		}
		// stars notify the repo owner directly, no fanout.
		i.notifyOne(ctx, recipientDid, actorDid, entityAt, "", repoDid, models.NotificationTypeRepoStarred, "")

	case tangled.GraphFollowNSID:
		var rec tangled.GraphFollow
		if err := json.Unmarshal(e.Commit.Record, &rec); err != nil {
			i.logger.Warn("decoding follow record", "err", err, "uri", entityAt)
			return nil
		}
		if rec.Subject == "" {
			return nil
		}
		i.notifyOne(ctx, rec.Subject, actorDid, entityAt, "", "", models.NotificationTypeFollowed, "")
	}

	return nil
}

func (i *Ingester) notifyEntity(ctx context.Context, actorDid, sourceAt, entityAt, repoDid string, t models.NotificationType, title string, mentions []string) {
	seen := make(map[string]struct{})

	subscribers, err := i.recipients.ListRecipients(ctx, entityAt)
	if err != nil {
		i.logger.Warn("listing recipients", "err", err, "entity", entityAt)
	}

	for _, dids := range [][]string{subscribers, mentions} {
		for _, did := range dids {
			if _, ok := seen[did]; ok {
				continue
			}
			seen[did] = struct{}{}
			i.deliver(did, actorDid, sourceAt, entityAt, repoDid, t, title)
		}
	}
}

// hydrateRepoOwner reads the repo cache, falling back to bobbin on a miss so
// stars on repos the ingester never saw still notify their owner. The resolved
// mapping is cached, so the cache fills in as repos are starred rather than
// needing a backfill.
func (i *Ingester) hydrateRepoOwner(ctx context.Context, repoDid string) string {
	if owner := deldb.GetRepoOwner(i.db, repoDid); owner != "" {
		return owner
	}
	if i.recipients == nil {
		return ""
	}

	owner, name, err := i.recipients.RepoOwner(ctx, repoDid)
	if err != nil {
		i.logger.Warn("resolving repo owner", "err", err, "repoDid", repoDid)
		return ""
	}
	if owner == "" {
		return ""
	}

	// a nameless record must not blank out a name we already cached.
	if name == "" {
		name = deldb.GetRepoName(i.db, repoDid)
	}
	if err := deldb.PutRepoName(i.db, repoDid, owner, name); err != nil {
		i.logger.Warn("caching repo owner", "err", err, "repoDid", repoDid)
	}
	return owner
}

func (i *Ingester) notifyOne(ctx context.Context, recipientDid, actorDid, sourceAt, entityAt, repoDid string, t models.NotificationType, title string) {
	i.deliver(recipientDid, actorDid, sourceAt, entityAt, repoDid, t, title)
}

func (i *Ingester) deliver(recipientDid, actorDid, sourceAt, entityAt, repoDid string, t models.NotificationType, title string) {
	if recipientDid == "" || recipientDid == actorDid {
		return
	}

	prefs, err := deldb.GetNotificationPreference(i.db, recipientDid)
	if err != nil {
		i.logger.Warn("loading prefs", "err", err, "recipient", recipientDid)
		return
	}
	if !prefs.ShouldNotify(t) {
		return
	}

	n := &models.Notification{
		RecipientDid: recipientDid,
		AtUri:        sourceAt,
		Type:         t,
		ActorDid:     actorDid,
		RepoDid:      repoDid,
		EntityAt:     entityAt,
		EntityTitle:  title,
	}
	if err := deldb.CreateNotification(i.db, n); err != nil {
		i.logger.Warn("creating notification", "err", err, "recipient", recipientDid, "uri", sourceAt)
	}
}
