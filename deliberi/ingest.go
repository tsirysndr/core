package deliberi

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	deldb "tangled.org/core/deliberi/db"
	models "tangled.org/core/deliberi/models"
	"tangled.org/core/idresolver"
	js "tangled.org/core/jetstream"
)

type Ingester struct {
	db         *deldb.DB
	recipients recipientResolver
	jc         *js.JetstreamClient
	idResolver *idresolver.Resolver
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

func NewIngester(database *deldb.DB, recipients recipientResolver, idRes *idresolver.Resolver, endpoint, ident string, logger *slog.Logger) (*Ingester, error) {
	jc, err := js.NewJetstreamClient(endpoint, ident, ingestCollections, nil, logger, database, false, false)
	if err != nil {
		return nil, fmt.Errorf("creating jetstream client: %w", err)
	}
	return &Ingester{
		db:         database,
		recipients: recipients,
		jc:         jc,
		idResolver: idRes,
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
		if err := deldb.PutEntityTitle(i.db, entityAt, rec.Title, rec.Repo); err != nil {
			i.logger.Warn("caching entity title", "err", err, "uri", entityAt)
		}
		i.notifyEntity(ctx, actorDid, entityAt, entityAt, rec.Repo, models.NotificationTypeIssueCreated, rec.Title, rec.Mentions, "sh.tangled.repo.issue")

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
		if err := deldb.PutEntityTitle(i.db, entityAt, rec.Title, repoDid); err != nil {
			i.logger.Warn("caching entity title", "err", err, "uri", entityAt)
		}
		i.notifyEntity(ctx, actorDid, entityAt, entityAt, repoDid, models.NotificationTypePullCreated, rec.Title, rec.Mentions, "sh.tangled.repo.pull")

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
		collection := syntax.ATURI(subjectUri).Collection().String()
		var t models.NotificationType
		switch collection {
		case tangled.RepoIssueNSID:
			t = models.NotificationTypeIssueCommented
		case tangled.RepoPullNSID:
			t = models.NotificationTypePullCommented
		default:
			return nil
		}
		// comments carry no mentions field.
		title, repoDid := i.hydrateEntity(ctx, subjectUri, collection)
		i.notifyEntity(ctx, actorDid, entityAt, subjectUri, repoDid, t, title, nil, collection)

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

func (i *Ingester) notifyEntity(ctx context.Context, actorDid, sourceAt, entityAt, repoDid string, t models.NotificationType, title string, mentions []string, collection string) {
	seen := make(map[string]struct{})

	// bobbin matches subjects exactly, so ask at both levels.
	var subscribers []string
	for _, subject := range []string{entityAt, repoDid} {
		if subject == "" {
			continue
		}
		dids, err := i.recipients.ListRecipients(ctx, subject, collection)
		if err != nil {
			i.logger.Warn("listing recipients", "err", err, "subject", subject)
			continue
		}
		subscribers = append(subscribers, dids...)
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

// hydrateEntity reads the entity cache, falling back to the parent's pds on a
// miss so comments on entities the ingester never saw still resolve.
func (i *Ingester) hydrateEntity(ctx context.Context, uri, collection string) (title, repoDid string) {
	title = deldb.GetEntityTitle(i.db, uri)
	repoDid = deldb.GetEntityRepo(i.db, uri)
	if repoDid != "" || i.idResolver == nil {
		return title, repoDid
	}

	fetchedTitle, fetchedRepoDid, err := i.fetchEntity(ctx, uri, collection)
	if err != nil {
		i.logger.Warn("hydrating parent entity", "err", err, "uri", uri)
		return title, repoDid
	}
	if title == "" {
		title = fetchedTitle
	}
	repoDid = fetchedRepoDid
	if err := deldb.PutEntityTitle(i.db, uri, title, repoDid); err != nil {
		i.logger.Warn("caching hydrated entity", "err", err, "uri", uri)
	}
	return title, repoDid
}

func (i *Ingester) fetchEntity(ctx context.Context, uri, collection string) (string, string, error) {
	at := syntax.ATURI(uri)
	ident, err := i.idResolver.ResolveIdent(ctx, at.Authority().String())
	if err != nil {
		return "", "", fmt.Errorf("resolving %s: %w", at.Authority(), err)
	}

	xc := &indigoxrpc.Client{
		Host:   ident.PDSEndpoint(),
		Client: &http.Client{Timeout: 10 * time.Second},
	}
	out, err := comatproto.RepoGetRecord(ctx, xc, "", collection, ident.DID.String(), at.RecordKey().String())
	if err != nil {
		return "", "", fmt.Errorf("getting record: %w", err)
	}
	if out == nil || out.Value == nil {
		return "", "", fmt.Errorf("record has no value")
	}
	raw, err := out.Value.MarshalJSON()
	if err != nil {
		return "", "", fmt.Errorf("re-encoding record: %w", err)
	}

	switch collection {
	case tangled.RepoIssueNSID:
		var rec tangled.RepoIssue
		if err := json.Unmarshal(raw, &rec); err != nil {
			return "", "", fmt.Errorf("decoding issue: %w", err)
		}
		return rec.Title, rec.Repo, nil
	case tangled.RepoPullNSID:
		var rec tangled.RepoPull
		if err := json.Unmarshal(raw, &rec); err != nil {
			return "", "", fmt.Errorf("decoding pull: %w", err)
		}
		if rec.Target == nil {
			return rec.Title, "", nil
		}
		return rec.Title, rec.Target.Repo, nil
	}
	return "", "", fmt.Errorf("unsupported collection %s", collection)
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
