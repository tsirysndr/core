package state

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview"
	"tangled.org/core/appview/bsky"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/email"
	"tangled.org/core/appview/indexer"
	"tangled.org/core/appview/mentions"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	dbnotify "tangled.org/core/appview/notify/db"
	lognotify "tangled.org/core/appview/notify/logging"
	phnotify "tangled.org/core/appview/notify/posthog"
	whnotify "tangled.org/core/appview/notify/webhook"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pipelines"
	pipelinessh "tangled.org/core/appview/pipelines/ssh"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/repoverify"
	"tangled.org/core/appview/validator"
	xrpcclient "tangled.org/core/appview/xrpcclient"
	"tangled.org/core/consts"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/idresolver"
	"tangled.org/core/jetstream"
	"tangled.org/core/log"
	tlog "tangled.org/core/log"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/bluesky-social/indigo/xrpc"

	"github.com/go-chi/chi/v5"
	"github.com/posthog/posthog-go"
)

type State struct {
	db               *db.DB
	notifier         notify.Notifier
	indexer          *indexer.Indexer
	oauth            *oauth.OAuth
	enforcer         *rbac.Enforcer
	pages            *pages.Pages
	idResolver       *idresolver.Resolver
	rdb              *cache.Cache
	mentionsResolver *mentions.Resolver
	posthog          posthog.Client
	jc               *jetstream.JetstreamClient
	config           *config.Config
	repoResolver     *reporesolver.RepoResolver
	knotstream       *eventconsumer.Consumer
	spindlestream    *eventconsumer.Consumer
	pipelineNotifier *pipelines.StatusNotifier
	logger           *slog.Logger
	validator        *validator.Validator
	cfClient         *cloudflare.Client
}

func Make(ctx context.Context, config *config.Config) (*State, error) {
	logger := tlog.FromContext(ctx)

	d, err := db.Make(ctx, config.Core.DbPath)
	if err != nil {
		return nil, fmt.Errorf("failed to create db: %w", err)
	}

	indexer := indexer.New(log.SubLogger(logger, "indexer"), d)
	err = indexer.Init(ctx)
	if err != nil {
		return nil, fmt.Errorf("failed to create indexer: %w", err)
	}

	enforcer, err := rbac.NewEnforcer(config.Core.DbPath)
	if err != nil {
		return nil, fmt.Errorf("failed to create enforcer: %w", err)
	}

	res, err := idresolver.RedisResolver(config.Redis.ToURL(), config.Plc.PLCURL)
	if err != nil {
		logger.Error("failed to create redis resolver", "err", err)
		res = idresolver.DefaultResolver(config.Plc.PLCURL)
	}

	var rdb *cache.Cache
	if config.Redis.Addr != "" {
		rdb = cache.New(config.Redis.Addr)
	}

	posthog, err := posthog.NewWithConfig(config.Posthog.ApiKey, posthog.Config{Endpoint: config.Posthog.Endpoint})
	if err != nil {
		return nil, fmt.Errorf("failed to create posthog client: %w", err)
	}

	pages := pages.NewPages(config, res, d, rdb, log.SubLogger(logger, "pages"))
	oauth, err := oauth.New(config, posthog, d, enforcer, res, log.SubLogger(logger, "oauth"))
	if err != nil {
		return nil, fmt.Errorf("failed to start oauth handler: %w", err)
	}
	validator := validator.New(d, res, enforcer)

	repoResolver := reporesolver.New(config, enforcer, d, rdb)

	mentionsResolver := mentions.New(config, res, d, log.SubLogger(logger, "mentionsResolver"))

	jc, err := jetstream.NewJetstreamClient(
		config.Jetstream.Endpoint,
		"appview",
		[]string{
			tangled.ActorProfileNSID,
			tangled.FeedStarNSID,
			tangled.FeedReactionNSID,
			tangled.FeedCommentNSID,
			tangled.GraphFollowNSID,
			tangled.GraphVouchNSID,
			tangled.KnotMemberNSID,
			tangled.KnotNSID,
			tangled.LabelDefinitionNSID,
			tangled.LabelOpNSID,
			tangled.PublicKeyNSID,
			tangled.RepoArtifactNSID,
			tangled.RepoIssueCommentNSID,
			tangled.RepoIssueNSID,
			tangled.RepoNSID,
			tangled.RepoPullNSID,
			tangled.RepoPullCommentNSID,
			tangled.SpindleMemberNSID,
			tangled.SpindleNSID,
			tangled.StringNSID,
		},
		nil,
		tlog.SubLogger(logger, "jetstream"),
		d,
		false,

		// in-memory filter is inapplicable to appview so
		// we'll never log dids anyway.
		false,
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create jetstream client: %w", err)
	}

	if err := BackfillDefaultDefs(d, res, config.Label.DefaultLabelDefs); err != nil {
		return nil, fmt.Errorf("failed to backfill default label defs: %w", err)
	}

	var notifiers []notify.Notifier

	// Always add the database notifier
	notifiers = append(notifiers, dbnotify.NewDatabaseNotifier(d, res))

	// Add other notifiers in production only
	if !config.Core.Dev {
		notifiers = append(notifiers, phnotify.NewPosthogNotifier(posthog))
	}
	notifiers = append(notifiers, indexer)

	notifiers = append(notifiers, whnotify.NewNotifier(d))

	notifier := notify.NewMergedNotifier(notifiers)
	notifier = lognotify.NewLoggingNotifier(notifier, tlog.SubLogger(logger, "notify"))

	ingester := appview.Ingester{
		Ctx:              ctx,
		Db:               d,
		Enforcer:         enforcer,
		IdResolver:       res,
		Cache:            rdb,
		Config:           config,
		Logger:           log.SubLogger(logger, "ingester"),
		Validator:        validator,
		MentionsResolver: mentionsResolver,
		Notifier:         notifier,
		Verifier:         repoverify.New(res, config.Core.Dev),
	}
	err = jc.StartJetstream(ctx, ingester.Ingest())
	if err != nil {
		return nil, fmt.Errorf("failed to start jetstream watcher: %w", err)
	}

	go ingester.SweepPendingVerifications()

	var cfClient *cloudflare.Client
	if config.Cloudflare.ApiToken != "" {
		cfClient, err = cloudflare.New(config)
		if err != nil {
			logger.Warn("failed to create cloudflare client, sites upload will be disabled", "err", err)
			cfClient = nil
		}
	}

	knotstream, err := Knotstream(ctx, config, d, enforcer, posthog, notifier, cfClient)
	if err != nil {
		return nil, fmt.Errorf("failed to start knotstream consumer: %w", err)
	}
	knotstream.Start(ctx)

	pipelineNotifier := pipelines.NewStatusNotifier()

	spindlestream, err := Spindlestream(ctx, config, d, enforcer, pipelineNotifier)
	if err != nil {
		return nil, fmt.Errorf("failed to start spindlestream consumer: %w", err)
	}
	spindlestream.Start(ctx)

	state := &State{
		db:               d,
		notifier:         notifier,
		indexer:          indexer,
		oauth:            oauth,
		enforcer:         enforcer,
		pages:            pages,
		idResolver:       res,
		rdb:              rdb,
		mentionsResolver: mentionsResolver,
		posthog:          posthog,
		jc:               jc,
		config:           config,
		repoResolver:     repoResolver,
		knotstream:       knotstream,
		spindlestream:    spindlestream,
		pipelineNotifier: pipelineNotifier,
		logger:           logger,
		validator:        validator,
		cfClient:         cfClient,
	}

	// fetch initial bluesky posts if configured
	go fetchBskyPosts(ctx, res, config, d, logger)

	return state, nil
}

func (s *State) Close() error {
	// other close up logic goes here
	return s.db.Close()
}

func (s *State) NewSSHServer() *pipelinessh.Server {
	return pipelinessh.New(s.db, s.config, s.pipelineNotifier, log.SubLogger(s.logger, "pipelinessh"))
}

func (s *State) SecurityTxt(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain")
	w.Header().Set("Cache-Control", "public, max-age=86400") // one day

	securityTxt := `Contact: mailto:security@tangled.org
Preferred-Languages: en
Canonical: https://tangled.org/.well-known/security.txt
Expires: 2030-01-01T21:59:00.000Z
`
	w.Write([]byte(securityTxt))
}

func (s *State) RobotsTxt(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain")
	w.Header().Set("Cache-Control", "public, max-age=86400") // one day

	robotsTxt := `# Hello, Tanglers!
User-agent: *
Allow: /
Disallow: /*/*/settings
Disallow: /settings
Disallow: /*/*/compare
Disallow: /*/*/fork

Crawl-delay: 1
`
	w.Write([]byte(robotsTxt))
}

func (s *State) TermsOfService(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	s.pages.TermsOfService(w, pages.TermsOfServiceParams{
		LoggedInUser: user,
	})
}

func (s *State) PrivacyPolicy(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	s.pages.PrivacyPolicy(w, pages.PrivacyPolicyParams{
		LoggedInUser: user,
	})
}

func (s *State) Brand(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	s.pages.Brand(w, pages.BrandParams{
		LoggedInUser: user,
	})
}

func (s *State) UpgradeBanner(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		return
	}

	l := s.logger.With("handler", "UpgradeBanner")
	l = l.With("did", user.Did)

	regs, err := db.GetRegistrations(
		s.db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("needs_upgrade", 1),
	)
	if err != nil {
		l.Error("non-fatal: failed to get registrations", "err", err)
	}

	spindles, err := db.GetSpindles(
		r.Context(),
		s.db,
		orm.FilterEq("owner", user.Did),
		orm.FilterEq("needs_upgrade", 1),
	)
	if err != nil {
		l.Error("non-fatal: failed to get spindles", "err", err)
	}

	if regs == nil && spindles == nil {
		return
	}

	s.pages.UpgradeBanner(w, pages.UpgradeBannerParams{
		Registrations: regs,
		Spindles:      spindles,
	})
}

func (s *State) NewsletterSignup(w http.ResponseWriter, r *http.Request) {
	// target is echoed back from the form via hx-vals so the response span's
	// id matches the form's hx-target. Fallback keeps the handler useful if
	// a caller forgets to send it.
	target := strings.TrimSpace(r.FormValue("target"))
	if target == "" {
		target = "home"
	}

	w.Header().Set("Content-Type", "text/html")

	emailAddr := strings.TrimSpace(r.FormValue("email"))
	if !email.IsValidEmail(emailAddr) {
		s.pages.NewsletterResponse(w, pages.NewsletterResponseParams{
			Id:    target,
			Error: "Invalid email address.",
		})
		return
	}

	// For logged-in users, persist the signup locally so the widget stays
	// hidden across devices. The DB row is the render-time source of truth;
	// Resend still owns the mailing list itself.
	if user := s.oauth.GetMultiAccountUser(r); user != nil {
		if err := db.UpsertNewsletterPref(s.db, user.Did, db.NewsletterStatusSubscribed, emailAddr); err != nil {
			s.logger.Error("failed to persist newsletter preference", "did", user.Did, "err", err)
		}
	}

	if s.config.Resend.ApiKey != "" && s.config.Resend.NewsletterSegmentId != "" {
		go func() {
			if err := email.AddNewsletterContact(s.config.Resend.ApiKey, s.config.Resend.NewsletterSegmentId, emailAddr); err != nil {
				s.logger.Error("failed to add newsletter contact", "error", err)
			}
		}()
	} else {
		s.logger.Error(
			"failed to add newsletter contact, missing resend config",
			"isKeyPresent", s.config.Resend.ApiKey != "",
			"isSegmentIdPresent", s.config.Resend.NewsletterSegmentId != "",
			"emailAddr", emailAddr,
		)
	}

	s.pages.NewsletterResponse(w, pages.NewsletterResponseParams{Id: target})
}

// NewsletterDismiss records that a logged-in user has dismissed the newsletter
// widget so it stays hidden across their devices. Anonymous callers get a 204
// with no DB write — localStorage handles the per-browser fallback.
func (s *State) NewsletterDismiss(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	if err := db.UpsertNewsletterPref(s.db, user.Did, db.NewsletterStatusDismissed, ""); err != nil {
		s.logger.Error("failed to persist newsletter dismissal", "did", user.Did, "err", err)
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *State) Keys(w http.ResponseWriter, r *http.Request) {
	user := chi.URLParam(r, "user")
	user = strings.TrimPrefix(user, "@")

	if user == "" {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	id, err := s.idResolver.ResolveIdent(r.Context(), user)
	if err != nil {
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	pubKeys, err := db.GetPublicKeysForDid(s.db, id.DID.String())
	if err != nil {
		s.logger.Error("failed to get public keys", "err", err)
		http.Error(w, "failed to get public keys", http.StatusInternalServerError)
		return
	}

	if len(pubKeys) == 0 {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	for _, k := range pubKeys {
		key := strings.TrimRight(k.Key, "\n")
		fmt.Fprintln(w, key)
	}
}

func (s *State) NewRepo(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		user := s.oauth.GetMultiAccountUser(r)
		knots, err := s.enforcer.GetKnotsForUser(user.Did)
		if err != nil {
			s.pages.Notice(w, "repo", "Invalid user account.")
			return
		}

		s.pages.NewRepo(w, pages.NewRepoParams{
			LoggedInUser: user,
			Knots:        knots,
		})

	case http.MethodPost:
		l := s.logger.With("handler", "NewRepo")

		user := s.oauth.GetMultiAccountUser(r)
		l = l.With("did", user.Did)

		// form validation
		domain := r.FormValue("domain")
		if domain == "" {
			s.pages.Notice(w, "repo", "Invalid form submission&mdash;missing knot domain.")
			return
		}
		l = l.With("knot", domain)

		repoName := r.FormValue("name")
		if repoName == "" {
			s.pages.Notice(w, "repo", "Repository name cannot be empty.")
			return
		}

		if err := models.ValidateRepoName(repoName); err != nil {
			s.pages.Notice(w, "repo", err.Error())
			return
		}
		repoName = models.StripGitExt(repoName)
		rkey := strings.ToLower(repoName)
		l = l.With("repoName", repoName, "rkey", rkey)

		defaultBranch := r.FormValue("branch")
		if defaultBranch == "" {
			defaultBranch = "main"
		}
		l = l.With("defaultBranch", defaultBranch)

		description := r.FormValue("description")
		if len([]rune(description)) > 140 {
			s.pages.Notice(w, "repo", "Description must be 140 characters or fewer.")
			return
		}

		// ACL validation
		ok, err := s.enforcer.E.Enforce(user.Did, domain, domain, "repo:create")
		if err != nil || !ok {
			l.Info("unauthorized")
			s.pages.Notice(w, "repo", "You do not have permission to create a repo in this knot.")
			return
		}

		// Check for existing repos
		existingRepo, err := db.GetRepo(
			s.db,
			orm.FilterEq("did", user.Did),
			orm.FilterEq("rkey", rkey),
		)
		if err == nil && existingRepo != nil {
			l.Info("repo exists")
			s.pages.Notice(w, "repo", fmt.Sprintf("You already have a repository by this name on %s", existingRepo.Knot))
			return
		}

		atpClient, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			s.pages.Notice(w, "repo", "Failed to authorize. Try again later.")
			return
		}

		if rkeyOccupied(r.Context(), atpClient, user.Did, rkey) {
			l.Info("rkey occupied by prior rename alias")
			s.pages.Notice(w, "repo", fmt.Sprintf("The name %q still has a record on your PDS from a prior rename. Pick a different name, or delete at://%s/%s/%s first.", rkey, user.Did, tangled.RepoNSID, rkey))
			return
		}

		client, err := s.oauth.ServiceClient(
			r,
			oauth.WithService(domain),
			oauth.WithLxm(tangled.RepoCreateNSID),
			oauth.WithDev(s.config.Core.Dev),
		)
		if err != nil {
			l.Error("service auth failed", "err", err)
			s.pages.Notice(w, "repo", "Failed to authenticate. Please log out and log back in again.")
			return
		}

		input := &tangled.RepoCreate_Input{
			Rkey:          rkey,
			Name:          rkey,
			DefaultBranch: &defaultBranch,
		}
		createResp, err := tangled.RepoCreate(
			r.Context(),
			client,
			input,
		)
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.create", "xrpcerr", xrpcerr, "err", err)
			s.pages.Notice(w, "repo", err.Error())
			return
		}

		var repoDid string
		if createResp != nil && createResp.RepoDid != nil {
			repoDid = *createResp.RepoDid
		}
		if repoDid == "" {
			l.Error("knot returned empty repo DID")
			s.pages.Notice(w, "repo", "Knot failed to mint a repo DID. The knot may need to be upgraded.")
			return
		}

		repo := &models.Repo{
			Did:         user.Did,
			Name:        repoName,
			Knot:        domain,
			Rkey:        rkey,
			Description: description,
			Created:     time.Now(),
			Labels:      s.config.Label.DefaultLabelDefs,
			RepoDid:     repoDid,
		}
		record := repo.AsRecord()

		cleanupKnot := func() {
			go func() {
				delays := []time.Duration{0, 2 * time.Second, 5 * time.Second}
				for attempt, delay := range delays {
					time.Sleep(delay)
					deleteClient, dErr := s.oauth.ServiceClient(
						r,
						oauth.WithService(domain),
						oauth.WithLxm(tangled.RepoDeleteNSID),
						oauth.WithDev(s.config.Core.Dev),
					)
					if dErr != nil {
						l.Error("failed to create delete client for knot cleanup", "attempt", attempt+1, "err", dErr)
						continue
					}
					ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
					if dErr := tangled.RepoDelete(ctx, deleteClient, &tangled.RepoDelete_Input{
						Did:  user.Did,
						Name: rkey,
						Rkey: rkey,
					}); dErr != nil {
						cancel()
						l.Error("failed to clean up repo on knot after rollback", "attempt", attempt+1, "err", dErr)
						continue
					}
					cancel()
					l.Info("successfully cleaned up repo on knot after rollback", "attempt", attempt+1)
					return
				}
				l.Error("exhausted retries for knot cleanup, repo may be orphaned",
					"did", user.Did, "repo", repoName, "knot", domain)
			}()
		}

		_, err = comatproto.RepoCreateRecord(r.Context(), atpClient, &comatproto.RepoCreateRecord_Input{
			Collection: tangled.RepoNSID,
			Repo:       user.Did,
			Rkey:       &rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &record,
			},
		})
		if err != nil {
			l.Info("PDS write failed", "err", err)
			cleanupKnot()
			if rkeyOccupied(r.Context(), atpClient, user.Did, rkey) {
				s.pages.Notice(w, "repo", fmt.Sprintf("You already have a repository named %q.", rkey))
			} else {
				s.pages.Notice(w, "repo", "Failed to announce repository creation.")
			}
			return
		}

		aturi := fmt.Sprintf("at://%s/%s/%s", user.Did, tangled.RepoNSID, rkey)
		l = l.With("aturi", aturi)
		l.Info("wrote to PDS")

		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Info("txn failed", "err", err)
			s.pages.Notice(w, "repo", "Failed to save repository information.")
			return
		}

		rollback := func() {
			err1 := tx.Rollback()
			err2 := s.enforcer.E.LoadPolicy()
			err3 := rollbackRecord(context.Background(), aturi, atpClient)

			if errors.Is(err1, sql.ErrTxDone) {
				err1 = nil
			}

			if errs := errors.Join(err1, err2, err3); errs != nil {
				l.Error("failed to rollback changes", "errs", errs)
			}

			if aturi != "" {
				cleanupKnot()
			}
		}
		defer rollback()

		err = db.AddRepo(tx, repo)
		if err != nil {
			l.Error("db write failed", "err", err)
			s.pages.Notice(w, "repo", "Failed to save repository information.")
			return
		}

		rbacPath := repo.RepoIdentifier()
		err = s.enforcer.AddRepo(user.Did, domain, rbacPath)
		if err != nil {
			l.Error("acl setup failed", "err", err)
			s.pages.Notice(w, "repo", "Failed to set up repository permissions.")
			return
		}

		err = tx.Commit()
		if err != nil {
			l.Error("txn commit failed", "err", err)
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		err = s.enforcer.E.SavePolicy()
		if err != nil {
			l.Error("acl save failed", "err", err)
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		aturi = ""

		s.notifier.NewRepo(r.Context(), repo)
		switch {
		case repoDid != "":
			s.pages.HxLocation(w, fmt.Sprintf("/%s", repoDid))
		default:
			handle := s.pages.DisplayHandle(r.Context(), user.Did)
			s.pages.HxLocation(w, fmt.Sprintf("/%s/%s", handle, rkey))
		}
	}
}

func rkeyOccupied(ctx context.Context, client *atclient.APIClient, did, rkey string) bool {
	probeCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	resp, err := comatproto.RepoGetRecord(probeCtx, client, "", tangled.RepoNSID, did, rkey)
	return err == nil && resp != nil
}

// this is used to rollback changes made to the PDS
//
// it is a no-op if the provided ATURI is empty
func rollbackRecord(ctx context.Context, aturi string, client *atclient.APIClient) error {
	if aturi == "" {
		return nil
	}

	parsed := syntax.ATURI(aturi)

	collection := parsed.Collection().String()
	repo := parsed.Authority().String()
	rkey := parsed.RecordKey().String()

	_, err := comatproto.RepoDeleteRecord(ctx, client, &comatproto.RepoDeleteRecord_Input{
		Collection: collection,
		Repo:       repo,
		Rkey:       rkey,
	})
	return err
}

func BackfillDefaultDefs(e db.Execer, r *idresolver.Resolver, defaults []string) error {
	defaultLabels, err := db.GetLabelDefinitions(e, orm.FilterIn("at_uri", defaults))
	if err != nil {
		return err
	}
	// already present
	if len(defaultLabels) == len(defaults) {
		return nil
	}

	labelDefs, err := models.FetchLabelDefs(r, defaults)
	if err != nil {
		return err
	}

	// Insert each label definition to the database
	for _, labelDef := range labelDefs {
		_, err = db.AddLabelDefinition(e, &labelDef)
		if err != nil {
			return fmt.Errorf("failed to add label definition %s: %v", labelDef.Name, err)
		}
	}

	return nil
}

func fetchBskyPosts(ctx context.Context, res *idresolver.Resolver, config *config.Config, d *db.DB, logger *slog.Logger) {
	resolved, err := res.ResolveIdent(context.Background(), consts.TangledDid)
	if err != nil {
		logger.Error("failed to resolve tangled.org DID", "err", err)
		return
	}

	pdsEndpoint := resolved.PDSEndpoint()
	if pdsEndpoint == "" {
		logger.Error("no PDS endpoint found for tangled.sh DID")
		return
	}

	session, err := oauth.CreateAppPasswordSession(res, config.Core.AppPassword, consts.TangledDid, logger)
	if err != nil {
		logger.Error("failed to create appassword session... skipping fetch", "err", err)
		return
	}

	l := log.SubLogger(logger, "bluesky")

	ticker := time.NewTicker(config.Bluesky.UpdateInterval)
	defer ticker.Stop()

	for {
		// refresh session if necessary
		if !session.IsValid() {
			l.Debug("access token expired, refreshing session")
			if err := session.RefreshSession(); err != nil {
				l.Error("failed to refresh session, stopping bluesky updater", "err", err)
				return
			}
			l.Debug("session refreshed")
		}

		// make client
		client := xrpc.Client{
			Auth: &xrpc.AuthInfo{
				AccessJwt: session.AccessJwt,
				Did:       session.Did,
			},
			Host: session.PdsEndpoint,
		}

		posts, _, err := bsky.FetchPosts(ctx, &client, 20, "")
		if err != nil {
			l.Error("failed to fetch bluesky posts", "err", err)
		} else if err := db.InsertBlueskyPosts(d, posts); err != nil {
			l.Error("failed to insert bluesky posts", "err", err)
		} else {
			l.Info("inserted bluesky posts", "count", len(posts))
		}

		select {
		case <-ticker.C:
		case <-ctx.Done():
			l.Info("stopping bluesky updater")
			return
		}
	}
}
