package spindle

import (
	"context"
	_ "embed"
	"encoding/json"
	"fmt"
	"log/slog"
	"maps"
	"net/http"
	"path/filepath"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"github.com/hashicorp/go-version"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
	"tangled.org/core/idresolver"
	"tangled.org/core/jetstream"
	"tangled.org/core/log"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/engines/dummy"
	"tangled.org/core/spindle/engines/microvm"
	"tangled.org/core/spindle/engines/nixery"
	"tangled.org/core/spindle/git"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/queue"
	"tangled.org/core/spindle/secrets"
	"tangled.org/core/spindle/xrpc"
	"tangled.org/core/xrpc/serviceauth"
)

//go:embed motd
var defaultMotd []byte

const (
	rbacDomain = "thisserver"
)

type Spindle struct {
	jc       *jetstream.JetstreamClient
	tap      *Tap
	embedTap *embeddedTap
	db       *db.DB
	e        *rbac.Enforcer
	l        *slog.Logger
	n        *notifier.Notifier
	engs     map[string]models.Engine
	jq       *queue.Queue
	cfg      *config.Config
	ks       *eventconsumer.Consumer
	res      *idresolver.Resolver
	vault    secrets.Manager
	motd     []byte
	motdMu   sync.RWMutex
	rootCtx  context.Context
}

// New creates a new Spindle server with the provided configuration and engines.
func New(ctx context.Context, cfg *config.Config, d *db.DB, engines map[string]models.Engine) (*Spindle, error) {
	logger := log.FromContext(ctx)

	e, err := rbac.NewEnforcer(cfg.Server.DBPath)
	if err != nil {
		return nil, fmt.Errorf("failed to setup rbac enforcer: %w", err)
	}
	e.E.EnableAutoSave(true)

	n := notifier.New()

	var vault secrets.Manager
	switch cfg.Server.Secrets.Provider {
	case "openbao":
		if cfg.Server.Secrets.OpenBao.ProxyAddr == "" {
			return nil, fmt.Errorf("openbao proxy address is required when using openbao secrets provider")
		}
		vault, err = secrets.NewOpenBaoManager(
			cfg.Server.Secrets.OpenBao.ProxyAddr,
			logger,
			secrets.WithMountPath(cfg.Server.Secrets.OpenBao.Mount),
		)
		if err != nil {
			return nil, fmt.Errorf("failed to setup openbao secrets provider: %w", err)
		}
		logger.Info("using openbao secrets provider", "proxy_address", cfg.Server.Secrets.OpenBao.ProxyAddr, "mount", cfg.Server.Secrets.OpenBao.Mount)
	case "sqlite", "":
		vault, err = secrets.NewSQLiteManager(cfg.Server.DBPath, secrets.WithTableName("secrets"))
		if err != nil {
			return nil, fmt.Errorf("failed to setup sqlite secrets provider: %w", err)
		}
		logger.Info("using sqlite secrets provider", "path", cfg.Server.DBPath)
	default:
		return nil, fmt.Errorf("unknown secrets provider: %s", cfg.Server.Secrets.Provider)
	}

	if err := runStartupMigrations(ctx, d, cfg.Server.Tap.Embed, cfg.Server.Tap.DBPath, logger); err != nil {
		return nil, fmt.Errorf("failed to run startup migrations: %w", err)
	}

	jq := queue.NewQueue(cfg.Server.QueueSize, cfg.Server.MaxJobCount)
	logger.Info("initialized queue", "queueSize", cfg.Server.QueueSize, "numWorkers", cfg.Server.MaxJobCount)

	collections := []string{
		tangled.SpindleMemberNSID,
		tangled.RepoNSID,
		tangled.RepoCollaboratorNSID,
	}
	jc, err := jetstream.NewJetstreamClient(cfg.Server.JetstreamEndpoint, "spindle", collections, nil, log.SubLogger(logger, "jetstream"), d, true, true)
	if err != nil {
		return nil, fmt.Errorf("failed to setup jetstream client: %w", err)
	}
	jc.AddDid(cfg.Server.Owner)

	// Check if the spindle knows about any Dids;
	dids, err := d.GetAllDids()
	if err != nil {
		return nil, fmt.Errorf("failed to get all dids: %w", err)
	}
	for _, d := range dids {
		jc.AddDid(d)
	}

	knownRepos, err := d.AllRepos()
	if err != nil {
		return nil, fmt.Errorf("failed to get known repos: %w", err)
	}
	for _, r := range knownRepos {
		if r.Owner != "" {
			jc.AddDid(r.Owner.String())
		}
	}

	resolver := idresolver.DefaultResolver(cfg.Server.PlcUrl)

	spindle := &Spindle{
		jc:      jc,
		e:       e,
		db:      d,
		l:       logger,
		n:       &n,
		engs:    engines,
		jq:      jq,
		cfg:     cfg,
		res:     resolver,
		vault:   vault,
		motd:    defaultMotd,
		rootCtx: ctx,
	}

	err = e.AddSpindle(rbacDomain)
	if err != nil {
		return nil, fmt.Errorf("failed to set rbac domain: %w", err)
	}
	err = spindle.configureOwner()
	if err != nil {
		return nil, err
	}
	logger.Info("owner set", "did", cfg.Server.Owner)

	cursorStore, err := cursor.NewSQLiteStore(cfg.Server.DBPath)
	if err != nil {
		return nil, fmt.Errorf("failed to setup sqlite3 cursor store: %w", err)
	}

	err = jc.StartJetstream(ctx, spindle.ingest())
	if err != nil {
		return nil, fmt.Errorf("failed to start jetstream consumer: %w", err)
	}

	// for each incoming sh.tangled.pipeline, we execute
	// spindle.processPipeline, which in turn enqueues the pipeline
	// job in the above registered queue.
	ccfg := eventconsumer.NewConsumerConfig()
	ccfg.Logger = log.SubLogger(logger, "eventconsumer")
	ccfg.ProcessFunc = spindle.processPipeline
	ccfg.CursorStore = cursorStore
	if cfg.Server.Dev {
		ccfg.RetryInterval = 5 * time.Second
		ccfg.MaxRetryInterval = 10 * time.Second
	} else {
		ccfg.RetryInterval = 1 * time.Minute
		ccfg.MaxRetryInterval = 10 * time.Minute
	}
	knownKnots, err := d.Knots()
	if err != nil {
		return nil, err
	}
	for _, knot := range knownKnots {
		logger.Info("adding source start", "knot", knot)
		src := eventconsumer.NewKnotSource(knot)
		eventconsumer.MigrateLegacyCursor(cursorStore, src)
		ccfg.Sources[src] = struct{}{}
	}
	spindle.ks = eventconsumer.NewConsumer(*ccfg)

	if cfg.Server.Tap.Embed {
		pw, err := randomAdminPassword()
		if err != nil {
			return nil, err
		}
		cfg.Server.Tap.AdminPassword = pw
		logger.Info("embedded tap: using random admin password")
	}
	spindle.tap = NewTapClient(spindle)

	return spindle, nil
}

// DB returns the database instance.
func (s *Spindle) DB() *db.DB {
	return s.db
}

// Queue returns the job queue instance.
func (s *Spindle) Queue() *queue.Queue {
	return s.jq
}

// Engines returns the map of available engines.
func (s *Spindle) Engines() map[string]models.Engine {
	return s.engs
}

// Vault returns the secrets manager instance.
func (s *Spindle) Vault() secrets.Manager {
	return s.vault
}

// Notifier returns the notifier instance.
func (s *Spindle) Notifier() *notifier.Notifier {
	return s.n
}

// Enforcer returns the RBAC enforcer instance.
func (s *Spindle) Enforcer() *rbac.Enforcer {
	return s.e
}

// SetMotdContent sets custom MOTD content, replacing the embedded default.
func (s *Spindle) SetMotdContent(content []byte) {
	s.motdMu.Lock()
	defer s.motdMu.Unlock()
	s.motd = content
}

// GetMotdContent returns the current MOTD content.
func (s *Spindle) GetMotdContent() []byte {
	s.motdMu.RLock()
	defer s.motdMu.RUnlock()
	return s.motd
}

// Start starts the Spindle server (blocking).
func (s *Spindle) Start(ctx context.Context) error {
	// starts a job queue runner in the background
	s.jq.Start()
	defer s.jq.Stop()

	// Stop vault token renewal if it implements Stopper
	if stopper, ok := s.vault.(secrets.Stopper); ok {
		defer stopper.Stop()
	}

	tapCtx, tapCancel := context.WithCancel(ctx)

	if s.cfg.Server.Tap.Embed {
		emb, err := startEmbeddedTap(tapCtx, s.cfg, log.SubLogger(s.l, "embedtap"))
		if err != nil {
			tapCancel()
			return fmt.Errorf("starting embedded tap: %w", err)
		}
		s.embedTap = emb
		defer func() {
			tapCancel()
			s.embedTap.Shutdown()
		}()

		go s.watchTapDrain(tapCtx, tapCancel)
	} else {
		defer tapCancel()
	}

	go func() {
		s.l.Info("starting knot event consumer")
		s.ks.Start(ctx)
	}()

	s.l.Info("starting tap client", "url", s.cfg.Server.Tap.Url)
	s.tap.Start(tapCtx)

	s.l.Info("starting spindle server", "address", s.cfg.Server.ListenAddr)
	return http.ListenAndServe(s.cfg.Server.ListenAddr, s.Router())
}

func (s *Spindle) declareTapInterest(ctx context.Context) {
	repos, err := s.db.AllRepos()
	if err != nil {
		s.l.Warn("tap declare: failed to load known repos", "err", err)
		return
	}
	seen := make(map[syntax.DID]struct{}, len(repos))
	dids := make([]syntax.DID, 0, len(repos))
	for _, r := range repos {
		if r.Owner == "" {
			continue
		}
		if _, ok := seen[r.Owner]; ok {
			continue
		}
		seen[r.Owner] = struct{}{}
		dids = append(dids, r.Owner)
	}
	if err := s.tap.AddOwnerDIDs(ctx, dids); err != nil {
		s.l.Warn("tap declare: AddRepos rejected", "count", len(dids), "err", err)
		return
	}
	s.l.Info("tap declare: known owner DIDs registered", "count", len(dids))
}

func Run(ctx context.Context) error {
	cfg, err := config.Load(ctx)
	if err != nil {
		return fmt.Errorf("failed to load config: %w", err)
	}

	if err := ensureGitVersion(); err != nil {
		return fmt.Errorf("ensuring git version: %w", err)
	}

	d, err := db.Make(ctx, cfg.Server.DBPath)
	if err != nil {
		return fmt.Errorf("failed to setup db: %w", err)
	}

	nixeryEng, err := nixery.New(ctx, cfg)
	if err != nil {
		return err
	}

	microvmEng, err := microvm.New(ctx, cfg, d)
	if err != nil {
		return err
	}

	s, err := New(ctx, cfg, d, map[string]models.Engine{
		"nixery":  nixeryEng,
		"microvm": microvmEng,
		"dummy":   dummy.New(log.FromContext(ctx)),
	})
	if err != nil {
		return err
	}

	return s.Start(ctx)
}

func (s *Spindle) Router() http.Handler {
	mux := chi.NewRouter()

	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Write(s.GetMotdContent())
	})
	mux.HandleFunc("/events", s.Events)
	mux.HandleFunc("/logs/{knot}/{rkey}/{name}", s.Logs)

	mux.Mount("/xrpc", s.XrpcRouter())
	return mux
}

func (s *Spindle) XrpcRouter() http.Handler {
	serviceAuth := serviceauth.NewServiceAuth(s.l, s.res.Directory(), s.cfg.Server.Did().String())

	l := log.SubLogger(s.l, "xrpc")

	x := xrpc.Xrpc{
		Logger:      l,
		Db:          s.db,
		Enforcer:    s.e,
		Engines:     s.engs,
		Config:      s.cfg,
		Resolver:    s.res,
		Vault:       s.vault,
		Notifier:    s.Notifier(),
		ServiceAuth: serviceAuth,
	}

	return x.Router()
}

func (s *Spindle) processPipeline(ctx context.Context, src eventconsumer.Source, msg eventstream.Event) error {
	l := log.FromContext(ctx).With("handler", "processKnotStream")
	l = l.With("src", src.Key(), "msg.Nsid", msg.Nsid, "msg.Rkey", msg.Rkey)
	if msg.Nsid == tangled.PipelineNSID {
		return nil
		tpl := tangled.Pipeline{}
		err := json.Unmarshal(msg.EventJson, &tpl)
		if err != nil {
			s.l.Error("failed to unmarshal pipeline event", "err", err)
			return err
		}

		if tpl.TriggerMetadata == nil {
			return fmt.Errorf("no trigger metadata found")
		}

		if tpl.TriggerMetadata.Repo == nil {
			return fmt.Errorf("no repo data found")
		}

		if src.Host != tpl.TriggerMetadata.Repo.Knot {
			return fmt.Errorf("repo knot does not match event source: %s != %s", src.Host, tpl.TriggerMetadata.Repo.Knot)
		}

		repoDid, err := s.resolvePipelineRepoDid(tpl.TriggerMetadata.Repo)
		if err != nil {
			return err
		}

		pipelineId := models.PipelineId{
			Knot: src.Host,
			Rkey: msg.Rkey,
		}

		workflows := make(map[models.Engine][]models.Workflow)

		// Build pipeline environment variables once for all workflows
		pipelineEnv := models.PipelineEnvVars(tpl.TriggerMetadata, pipelineId)

		for _, w := range tpl.Workflows {
			if w != nil {
				if _, ok := s.engs[w.Engine]; !ok {
					s.l.Error("workflow failed: unknown engine",
						"pipeline", pipelineId, "workflow", w.Name, "engine", w.Engine)
					err = s.db.StatusFailed(models.WorkflowId{
						PipelineId: pipelineId,
						Name:       w.Name,
					}, fmt.Sprintf("unknown engine %#v", w.Engine), -1, s.n)
					if err != nil {
						return fmt.Errorf("db.StatusFailed: %w", err)
					}

					continue
				}

				eng := s.engs[w.Engine]

				if _, ok := workflows[eng]; !ok {
					workflows[eng] = []models.Workflow{}
				}

				ewf, err := s.engs[w.Engine].InitWorkflow(*w, tpl)
				if err != nil {
					s.l.Error("workflow failed: init workflow",
						"pipeline", pipelineId, "workflow", w.Name, "engine", w.Engine, "err", err)
					err = s.db.StatusFailed(models.WorkflowId{
						PipelineId: pipelineId,
						Name:       w.Name,
					}, fmt.Sprintf("init workflow: %s", err), -1, s.n)
					if err != nil {
						return fmt.Errorf("db.StatusFailed: %w", err)
					}

					continue
				}

				// inject TANGLED_* env vars after InitWorkflow
				// This prevents user-defined env vars from overriding them
				if ewf.Environment == nil {
					ewf.Environment = make(map[string]string)
				}
				maps.Copy(ewf.Environment, pipelineEnv)

				workflows[eng] = append(workflows[eng], *ewf)

				err = s.db.StatusPending(models.WorkflowId{
					PipelineId: pipelineId,
					Name:       w.Name,
				}, s.n)
				if err != nil {
					return fmt.Errorf("db.StatusPending: %w", err)
				}
			}
		}

		ok := s.jq.Enqueue(repoDid, queue.Job{
			Run: func() error {
				engine.StartWorkflows(log.SubLogger(s.l, "engine"), s.vault, s.cfg, s.db, s.n, ctx, &models.Pipeline{
					RepoDid:   repoDid,
					Workflows: workflows,
				}, pipelineId)
				return nil
			},
			OnFail: func(jobError error) {
				s.l.Error("pipeline run failed", "error", jobError)
			},
		})
		if ok {
			s.l.Info("pipeline enqueued successfully", "id", msg.Rkey)
		} else {
			s.l.Error("failed to enqueue pipeline: queue is full")
		}
	} else if msg.Nsid == tangled.GitRefUpdateNSID {
		event := tangled.GitRefUpdate{}
		if err := json.Unmarshal(msg.EventJson, &event); err != nil {
			l.Error("error unmarshalling", "err", err)
			return err
		}
		l = l.With("repo", event.Repo, "ref", event.Ref, "newSha", event.NewSha)
		l.Debug("debug")

		repoDid := syntax.DID(event.Repo)
		if _, err := s.db.GetRepoByDid(repoDid); err != nil {
			return fmt.Errorf("unknown repoDid %s: %w", repoDid, err)
		}

		// NOTE: we are blindly trusting the knot that it will return only repos it own
		repoCloneUri := s.newRepoCloneUrl(src.Key(), syntax.DID(event.Repo))
		repoPath := s.newRepoPath(syntax.DID(event.Repo))
		if err := git.SparseSyncGitRepo(ctx, repoCloneUri, repoPath, event.NewSha); err != nil {
			return fmt.Errorf("sync git repo: %w", err)
		}
		l.Info("synced git repo")

		// TODO: plan the pipeline
	}

	return nil
}

// newRepoPath creates a path to store repository by its did and rkey.
// The path format would be: `/data/repos/did:plc:foo/sh.tangled.repo/repo-rkey
func (s *Spindle) newRepoPath(repo syntax.DID) string {
	return filepath.Join(s.cfg.Server.RepoDir, repo.String())
}

func (s *Spindle) newRepoCloneUrl(knot string, did syntax.DID) string {
	scheme := "https://"
	if s.cfg.Server.Dev {
		scheme = "http://"
	}
	return fmt.Sprintf("%s%s/%s", scheme, knot, did)
}

const RequiredVersion = "2.49.0"

func ensureGitVersion() error {
	v, err := git.Version()
	if err != nil {
		return fmt.Errorf("fetching git version: %w", err)
	}
	if v.LessThan(version.Must(version.NewVersion(RequiredVersion))) {
		return fmt.Errorf("installed git version %q is not supported, Spindle requires git version >= %q", v, RequiredVersion)
	}
	return nil
}

func (s *Spindle) resolvePipelineRepoDid(repo *tangled.Pipeline_TriggerRepo) (syntax.DID, error) {
	if repo.RepoDid == nil || *repo.RepoDid == "" {
		return "", fmt.Errorf("pipeline trigger missing repoDid")
	}
	repoDid, err := syntax.ParseDID(*repo.RepoDid)
	if err != nil {
		return "", fmt.Errorf("parse repoDid %s: %w", *repo.RepoDid, err)
	}
	if _, err := s.db.GetRepoByDid(repoDid); err != nil {
		return "", fmt.Errorf("unknown repoDid %s: %w", repoDid, err)
	}
	return repoDid, nil
}

func (s *Spindle) configureOwner() error {
	cfgOwner := s.cfg.Server.Owner

	existing, err := s.e.GetSpindleUsersByRole("server:owner", rbacDomain)
	if err != nil {
		return err
	}

	switch len(existing) {
	case 0:
		// no owner configured, continue
	case 1:
		// find existing owner
		existingOwner := existing[0]

		// no ownership change, this is okay
		if existingOwner == s.cfg.Server.Owner {
			break
		}

		// remove existing owner
		err = s.e.RemoveSpindleOwner(rbacDomain, existingOwner)
		if err != nil {
			return nil
		}
	default:
		return fmt.Errorf("more than one owner in DB, try deleting %q and starting over", s.cfg.Server.DBPath)
	}

	return s.e.AddSpindleOwner(rbacDomain, cfgOwner)
}
