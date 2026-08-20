package spindle

import (
	"context"
	"crypto/sha256"
	"database/sql"
	_ "embed"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"net/http"
	"path/filepath"
	"sort"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/hashicorp/go-version"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/eventstream"
	"tangled.org/core/idresolver"
	"tangled.org/core/jetstream"
	knotdb "tangled.org/core/knotserver/db"
	kgit "tangled.org/core/knotserver/git"
	"tangled.org/core/log"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/repoident"
	"tangled.org/core/repoverify"
	"tangled.org/core/spindle/artifactstore"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/engines/dagger"
	"tangled.org/core/spindle/engines/dummy"
	"tangled.org/core/spindle/engines/nixery"
	"tangled.org/core/spindle/git"
	"tangled.org/core/spindle/mill"
	"tangled.org/core/spindle/mill/executor"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
	"tangled.org/core/spindle/xrpc"
	"tangled.org/core/tid"
	"tangled.org/core/workflow"
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
	jobWake  chan struct{}
	cfg      *config.Config
	ks       *eventconsumer.Consumer
	res      *idresolver.Resolver
	verify   repoverify.Verifier
	vault    secrets.Manager
	motd     []byte
	motdMu   sync.RWMutex
	rootCtx  context.Context
	store    artifactstore.Store
	stores   *artifactstore.Stores
	reader   artifactstore.Reader
	// set only when this spindle hosts the mill or joins one as an executor
	mill *mill.Mill
	exec *executor.Executor
}

// New creates a new Spindle server with the provided configuration and engines.
func New(ctx context.Context, cfg *config.Config, d *db.DB, engines map[string]models.Engine) (*Spindle, error) {
	logger := log.FromContext(ctx)
	n := notifier.New()

	if cfg.Role == config.RoleExecutor {
		if err := cleanupOrphanRepos(ctx, d, logger); err != nil {
			return nil, fmt.Errorf("failed to run startup cleanup: %w", err)
		}
	} else if err := runStartupMigrations(ctx, d, cfg.Server.Tap.Embed, cfg.Server.Tap.DBPath, logger); err != nil {
		return nil, fmt.Errorf("failed to run startup migrations: %w", err)
	}

	spindle := &Spindle{
		db:      d,
		l:       logger,
		n:       &n,
		engs:    engines,
		cfg:     cfg,
		motd:    defaultMotd,
		rootCtx: ctx,
		jobWake: make(chan struct{}, 1),
	}
	diskFallback := ""
	if cfg.Role == config.RoleStandalone {
		diskFallback = cfg.Server.LogDir
		if cfg.ArtifactStores.Disk.Dir == "" {
			logger.Warn("using SPINDLE_SERVER_LOG_DIR as the implicit disk artifact store; configure SPINDLE_ARTIFACT_STORES_DISK_DIR explicitly")
		}
	}
	stores, err := artifactstore.NewStores(cfg.ArtifactStores, diskFallback, cfg.LegacyS3.LogBucket)
	if err != nil {
		return nil, fmt.Errorf("failed to setup artifact stores: %w", err)
	}
	spindle.stores = stores
	if cfg.LegacyS3.LogBucket != "" {
		logger.Warn("SPINDLE_S3_LOG_BUCKET is deprecated; use SPINDLE_ARTIFACT_STORES_S3_BUCKET")
	}
	if cfg.Role == config.RoleStandalone {
		spindle.reader = stores
	} else {
		name := cfg.Mill.ArtifactStore
		if name == "" {
			names := stores.Names()
			if len(names) != 1 {
				return nil, fmt.Errorf("%s requires SPINDLE_MILL_ARTIFACT_STORE when %d artifact stores are configured", cfg.Role, len(names))
			}
			name = names[0]
			logger.Warn("SPINDLE_MILL_ARTIFACT_STORE is not set; inferred the only configured store", "store", name)
		}
		store, ok := stores.Store(name)
		if !ok {
			return nil, fmt.Errorf("SPINDLE_MILL_ARTIFACT_STORE=%q is not configured", name)
		}
		spindle.store = store
		spindle.reader = store
	}
	if cfg.Role == config.RoleExecutor {
		return spindle, nil
	}

	e, err := rbac.NewEnforcer(cfg.Server.DBPath)
	if err != nil {
		return nil, fmt.Errorf("failed to setup rbac enforcer: %w", err)
	}
	e.E.EnableAutoSave(true)
	spindle.e = e

	switch cfg.Server.Secrets.Provider {
	case "openbao":
		if cfg.Server.Secrets.OpenBao.ProxyAddr == "" {
			return nil, fmt.Errorf("openbao proxy address is required when using openbao secrets provider")
		}
		spindle.vault, err = secrets.NewOpenBaoManager(
			cfg.Server.Secrets.OpenBao.ProxyAddr,
			logger,
			secrets.WithMountPath(cfg.Server.Secrets.OpenBao.Mount),
		)
		if err != nil {
			return nil, fmt.Errorf("failed to setup openbao secrets provider: %w", err)
		}
		logger.Info("using openbao secrets provider", "proxy_address", cfg.Server.Secrets.OpenBao.ProxyAddr, "mount", cfg.Server.Secrets.OpenBao.Mount)
	case "sqlite", "":
		spindle.vault, err = secrets.NewSQLiteManager(cfg.Server.DBPath, secrets.WithTableName("secrets"))
		if err != nil {
			return nil, fmt.Errorf("failed to setup sqlite secrets provider: %w", err)
		}
		logger.Info("using sqlite secrets provider", "path", cfg.Server.DBPath)
	default:
		return nil, fmt.Errorf("unknown secrets provider: %s", cfg.Server.Secrets.Provider)
	}

	collections := []string{
		tangled.SpindleMemberNSID,
		tangled.RepoNSID,
		tangled.RepoCollaboratorNSID,
		tangled.RepoPullNSID,
		tangled.RepoPullStatusNSID,
	}
	jc, err := jetstream.NewJetstreamClient(cfg.Server.JetstreamEndpoint, "spindle", collections, nil, log.SubLogger(logger, "jetstream"), d, true, true)
	if err != nil {
		return nil, fmt.Errorf("failed to setup jetstream client: %w", err)
	}
	spindle.jc = jc
	jc.AddDid(cfg.Server.Owner)
	// pull (status) records are created by arbitrary users too, same hack as in tap
	jc.ExemptCollection(tangled.RepoPullNSID)
	jc.ExemptCollection(tangled.RepoPullStatusNSID)

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

	spindle.res = idresolver.DefaultResolver(cfg.Server.PlcUrl)
	spindle.verify = repoverify.New(spindle.res, cfg.Server.Dev)

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

	// spindle listen to knot stream for sh.tangled.git.refUpdate
	// which will sync the local workflow files in spindle and enqueues the
	// pipeline job for on-push workflows
	ccfg := eventconsumer.NewConsumerConfig()
	ccfg.Logger = log.SubLogger(logger, "eventconsumer")
	ccfg.ProcessFunc = spindle.processKnotStream
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
func (s *Spindle) DB() *db.DB {
	return s.db
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

// runs the server. blocks
func (s *Spindle) Start(ctx context.Context) error {
	// only standalone runs the local queue. mill hosts place directly onto
	// executors, and executors only run jobs explicitly assigned by a mill
	s.StartJobWorkers(ctx)

	// an executor dials out to its mill and takes work from it
	if s.exec != nil {
		go s.exec.Connect(ctx)
	}

	if s.mill != nil && s.cfg.Mill.JumpListenAddr != "" {
		go s.mill.ServeJump(ctx, s.cfg.Mill.JumpListenAddr, s.cfg.Mill.JumpHostKeyPath, s.cfg.Mill.DebugExecutorPort, s.cfg.Mill.MaxJumpConnections)
	}

	if stopper, ok := s.vault.(secrets.Stopper); ok {
		defer stopper.Stop()
	}

	if s.cfg.Role != config.RoleExecutor {
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
	}

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

	logger := log.FromContext(ctx)

	var engines map[string]models.Engine
	var m *mill.Mill

	if cfg.Role == config.RoleMill {
		// on a mill host, engines place jobs on executors instead of running
		// them. all names share one Mill
		m = mill.New(log.SubLogger(logger, "mill"), mill.Config{
			LogDir:         cfg.Server.LogDir,
			MaxPending:     cfg.Mill.MaxPending,
			ReconnectGrace: cfg.Mill.ReconnectGrace,
		})
		engines = map[string]models.Engine{
			"nixery":  mill.NewEngine("nixery", m),
			"microvm": mill.NewEngine("microvm", m),
			"dagger":  mill.NewEngine("dagger", m),
			"dummy":   mill.NewEngine("dummy", m),
		}
	} else {
		// standalone and executor both run real engines locally
		nixeryEng, err := nixery.New(ctx, cfg)
		if err != nil {
			return err
		}
		microvmEng, err := newMicrovmEngine(ctx, cfg, d)
		if err != nil {
			return err
		}
		daggerEng, err := dagger.New(ctx, cfg)
		if err != nil {
			return err
		}
		engines = map[string]models.Engine{
			"nixery":  nixeryEng,
			"microvm": microvmEng,
			"dagger":  daggerEng,
			"dummy":   dummy.New(logger),
		}
	}

	s, err := New(ctx, cfg, d, engines)
	if err != nil {
		return err
	}

	if m != nil {
		// the engines built above hold the mill, but the mill's db and
		// notifier only exist after New, so attach them here
		m.Attach(s.DB(), s.Notifier())
		s.mill = m
		if err := m.RestoreState(); err != nil {
			return fmt.Errorf("restoring mill state: %w", err)
		}
	}
	if cfg.Role == config.RoleExecutor {
		s.exec, err = executor.New(cfg, engines, s.DB(), s.Notifier(), log.SubLogger(logger, "executor"), s.store)
		if err != nil {
			return err
		}
	}

	return s.Start(ctx)
}

func (s *Spindle) Router() http.Handler {
	mux := chi.NewRouter()

	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Write(s.GetMotdContent())
	})
	if s.cfg.Role == config.RoleExecutor {
		return mux
	}

	mux.HandleFunc("/events", s.Events)
	mux.HandleFunc("/logs/{knot}/{rkey}/{name}", s.Logs)

	// on a mill host, executors dial in here (plain ws, shared-secret auth)
	if s.mill != nil {
		mux.HandleFunc("/mill", s.mill.HandleExecutorConn)
	}

	mux.Mount("/xrpc", s.XrpcRouter())
	return mux
}

func (s *Spindle) XrpcRouter() http.Handler {
	serviceAuth := serviceauth.NewServiceAuth(s.l, s.res.Directory(), s.cfg.Server.Did().String())

	l := log.SubLogger(s.l, "xrpc")

	x := xrpc.Xrpc{
		Logger:         l,
		Db:             s.db,
		Enforcer:       s.e,
		Engines:        s.engs,
		Config:         s.cfg,
		ArtifactReader: s.reader,
		Resolver:       s.res,
		Vault:          s.vault,
		Notifier:       s.Notifier(),
		ServiceAuth:    serviceAuth,
		Trigger:        s,
	}

	return x.Router()
}

func (s *Spindle) processKnotStream(ctx context.Context, src eventconsumer.Source, msg eventstream.Event) error {
	l := log.FromContext(ctx).With("handler", "processKnotStream")
	l = l.With("src", src.Key(), "msg.Nsid", msg.Nsid, "msg.Rkey", msg.Rkey)
	if msg.Nsid == knotdb.RepoCollaboratorUpdateNSID {
		return s.ingestKnotCollaborator(ctx, l, src, msg)
	}
	if msg.Nsid == tangled.GitRefUpdateNSID {
		event := tangled.GitRefUpdate{}
		if err := json.Unmarshal(msg.EventJson, &event); err != nil {
			l.Error("error unmarshalling", "err", err)
			return err
		}
		l = l.With("repo", event.Repo, "ref", event.Ref, "newSha", event.NewSha)
		l.Debug("debug")

		repoDid := syntax.DID(event.Repo)
		repo, err := s.db.GetRepoByDid(repoDid)
		if err != nil {
			return fmt.Errorf("unknown repoDid %s: %w", repoDid, err)
		}

		if src.Host != repo.Knot {
			return fmt.Errorf("repo knot does not match event source: %s != %s", src.Host, repo.Knot)
		}

		if kgit.HasSkipCIPushOption(event.PushOptions) {
			l.Info("push event requested ci skip, skipping the event")
			return nil
		}

		// NOTE: we are blindly trusting the knot that it will return only repos it own
		repoCloneUri := s.newRepoCloneUrl(src.Host, repoDid)
		repoPath := s.newRepoPath(repoDid)
		if err := git.SparseSyncGitRepo(ctx, repoCloneUri, repoPath, event.NewSha); err != nil {
			return fmt.Errorf("sync git repo: %w", err)
		}
		l.Info("synced git repo")

		triggerRepo, err := s.buildTriggerRepo(ctx, repo)
		if err != nil {
			return fmt.Errorf("building trigger repo: %w", err)
		}

		trigger := tangled.Pipeline_TriggerMetadata{
			Kind: string(workflow.TriggerKindPush),
			Push: &tangled.Pipeline_PushTriggerData{
				Ref:    event.Ref,
				OldSha: event.OldSha,
				NewSha: event.NewSha,
			},
			Repo: triggerRepo,
		}

		pipelineId, err := s.runPipeline(ctx, repoDid, trigger, event.ChangedFiles, repoCloneUri, repoPath, event.NewSha, nil, triggerRepo)
		if err != nil {
			return err
		}
		if pipelineId.Rkey == "" {
			l.Info("no workflow matched 'push' trigger, skipping the event")
			return nil
		}
		l.Info("pipeline triggered", "pipeline", pipelineId.AtUri())
	}

	return nil
}

func (s *Spindle) ingestKnotCollaborator(ctx context.Context, l *slog.Logger, src eventconsumer.Source, msg eventstream.Event) error {
	var rec knotdb.RepoCollaboratorUpdate
	if err := json.Unmarshal(msg.EventJson, &rec); err != nil {
		l.Error("error unmarshalling collaboratorUpdate", "err", err)
		return err
	}

	subject, err := syntax.ParseDID(rec.Subject)
	if err != nil {
		l.Info("skipping collaboratorUpdate with malformed subject", "subject", rec.Subject, "err", err)
		return nil
	}
	repoDid, err := syntax.ParseDID(rec.Repo)
	if err != nil {
		l.Info("skipping collaboratorUpdate with malformed repo", "repo", rec.Repo, "err", err)
		return nil
	}

	repo, err := s.db.GetRepoByDid(repoDid)
	if errors.Is(err, sql.ErrNoRows) {
		l.Info("skipping collaboratorUpdate for unknown repo", "repo", repoDid)
		return nil
	}
	if err != nil {
		return fmt.Errorf("lookup repo %s: %w", repoDid, err)
	}
	if src.Host != repo.Knot {
		l.Warn("dropping collaboratorUpdate from non-owning knot", "src", src.Host, "repoKnot", repo.Knot)
		return nil
	}

	switch rec.Op {
	case knotdb.AclOpAdd:
		if err := s.e.AddCollaborator(subject.String(), rbac.ThisServer, repoDid.String()); err != nil {
			return fmt.Errorf("add collaborator policy: %w", err)
		}
		if err := s.db.AddKnotCollaborator(repoDid, subject); err != nil {
			return fmt.Errorf("track collaborator: %w", err)
		}
		l.Info("added knot-managed collaborator", "subject", subject, "repo", repoDid)
	case knotdb.AclOpRemove:
		if err := s.e.RemoveCollaborator(subject.String(), rbac.ThisServer, repoDid.String()); err != nil {
			return fmt.Errorf("remove collaborator policy: %w", err)
		}
		if err := s.db.DeleteRepoCollaboratorBySubjectRepo(subject, repoDid); err != nil {
			return fmt.Errorf("delete collaborator row: %w", err)
		}
		l.Info("removed knot-managed collaborator", "subject", subject, "repo", repoDid)
	default:
		return fmt.Errorf("collaboratorUpdate unknown op %q", rec.Op)
	}
	return nil
}

// buildTriggerRepo gathers trigger metadata, resolving default branch from the knot
func (s *Spindle) buildTriggerRepo(ctx context.Context, repo *db.Repo) (*tangled.Pipeline_TriggerRepo, error) {
	rkey := string(repo.Rkey)
	repoDid := repo.RepoDid.String()
	return s.buildTriggerRepoFrom(ctx, repo.Knot, repo.Owner.String(), rkey, repoDid), nil
}

func (s *Spindle) buildTriggerRepoFrom(ctx context.Context, knot, did, rkey, repoDid string) *tangled.Pipeline_TriggerRepo {
	scheme := "https"
	if s.cfg.Server.Dev {
		scheme = "http"
	}
	client := &indigoxrpc.Client{Host: fmt.Sprintf("%s://%s", scheme, knot)}

	// this should maybe (?) be in the refUpdate event itself to save a roundtrip
	defaultBranch := ""
	if out, err := tangled.RepoGetDefaultBranch(ctx, client, repoDid); err == nil {
		defaultBranch = out.Name
	}

	var rkeyPtr *string
	if rkey != "" {
		rkeyPtr = &rkey
	}
	return &tangled.Pipeline_TriggerRepo{
		Did:           did,
		Knot:          knot,
		Repo:          rkeyPtr,
		RepoDid:       &repoDid,
		DefaultBranch: defaultBranch,
	}
}

func (s *Spindle) resolvePipelineSourceRepo(ctx context.Context, trigger *tangled.Pipeline_TriggerMetadata) (*tangled.Pipeline_TriggerRepo, error) {
	if trigger == nil {
		return nil, nil
	}
	if trigger.SourceRepo == nil || *trigger.SourceRepo == "" {
		return trigger.Repo, nil
	}
	repoDid, err := syntax.ParseDID(*trigger.SourceRepo)
	if err != nil {
		return nil, fmt.Errorf("parse sourceRepo %s: %w", *trigger.SourceRepo, err)
	}
	return s.resolveSourceRepoInfo(ctx, repoDid)
}

// resolveSourceRepoInfo resolves trigger-repo metadata for a source repo DID.
func (s *Spindle) resolveSourceRepoInfo(ctx context.Context, repoDid syntax.DID) (*tangled.Pipeline_TriggerRepo, error) {
	repo, err := s.db.GetRepoByDid(repoDid)
	if err == nil {
		return s.buildTriggerRepo(ctx, repo)
	}

	// verify repo, we don't want git sync to point to arbitrary endpoints
	res, err := s.verify(ctx, repoident.RepoDid(repoDid))
	if err != nil {
		return nil, fmt.Errorf("verify sourceRepo %s: %w", repoDid, err)
	}
	ownership, ok := res.Ownership()
	if !ok {
		return nil, fmt.Errorf("verify sourceRepo %s: knot %s answered %s", repoDid, res.KnotURL, res.Answer())
	}
	return s.buildTriggerRepoFrom(ctx, res.KnotURL.Host(), ownership.OwnerDid.String(), ownership.Rkey.String(), repoDid.String()), nil
}

// runPipeline compiles and enqueues the pipeline for the given revision.
// sourceRepo is the resolved repo the code was checked out from, forwarded to
// processPipeline for env vars.
func (s *Spindle) runPipeline(ctx context.Context, repoDid syntax.DID, trigger tangled.Pipeline_TriggerMetadata, changedFiles []string, repoCloneUri, repoPath, rev string, only []string, sourceRepo *tangled.Pipeline_TriggerRepo) (models.PipelineId, error) {
	l := log.FromContext(ctx)

	compiler := workflow.Compiler{
		ChangedFiles: changedFiles,
		Trigger:      trigger,
	}

	rawPipeline, err := s.loadPipeline(ctx, repoCloneUri, repoPath, rev)
	if err != nil {
		return models.PipelineId{}, fmt.Errorf("loading pipeline: %w", err)
	}
	if len(rawPipeline) == 0 {
		return models.PipelineId{}, nil
	}

	tpl := compiler.Compile(compiler.Parse(rawPipeline))
	// todo(dawn): pass compile error to workflow log
	for _, w := range compiler.Diagnostics.Errors {
		l.Error(w.String())
	}
	for _, w := range compiler.Diagnostics.Warnings {
		l.Warn(w.String())
	}

	if len(only) > 0 {
		tpl.Workflows = filterWorkflows(tpl.Workflows, only)
	}
	if len(tpl.Workflows) == 0 {
		return models.PipelineId{}, nil
	}

	pipelineId := models.PipelineId{
		Knot: trigger.Repo.Knot,
		Rkey: tid.TID(),
	}
	if err := s.db.CreatePipelineEvent(pipelineId.Rkey, tpl, s.n); err != nil {
		return models.PipelineId{}, fmt.Errorf("creating pipeline event: %w", err)
	}
	err = s.processPipeline(repoDid, tpl, pipelineId, sourceRepo)
	return pipelineId, err
}

// filterWorkflows filters workflows to the requested names
func filterWorkflows(workflows []*tangled.Pipeline_Workflow, only []string) []*tangled.Pipeline_Workflow {
	allowed := make(map[string]struct{}, len(only))
	for _, n := range only {
		allowed[n] = struct{}{}
	}
	var filtered []*tangled.Pipeline_Workflow
	for _, w := range workflows {
		if w == nil {
			continue
		}
		if _, ok := allowed[w.Name]; ok {
			filtered = append(filtered, w)
		}
	}
	return filtered
}

// TriggerManual dispatches a pipeline at sha, authorized against and recorded
// under repoDid. sourceRepo, pull, and inputs are optional trigger payload.
func (s *Spindle) TriggerManual(ctx context.Context, repoDid syntax.DID, sha, ref string, workflows []string, sourceRepo syntax.DID, pull xrpc.PullContext, inputs []*tangled.Pipeline_Pair) (syntax.ATURI, error) {
	repo, err := s.db.GetRepoByDid(repoDid)
	if err != nil {
		return "", fmt.Errorf("unknown repoDid %s: %w", repoDid, err)
	}

	triggerRepo, err := s.buildTriggerRepo(ctx, repo)
	if err != nil {
		return "", fmt.Errorf("building trigger repo: %w", err)
	}

	trigger := tangled.Pipeline_TriggerMetadata{Repo: triggerRepo}
	if pull.IsPullRequest {
		var pullAt *string
		if pull.Pull != "" {
			pullAtStr := pull.Pull.String()
			pullAt = &pullAtStr
		}
		trigger.Kind = string(workflow.TriggerKindPullRequest)
		trigger.PullRequest = &tangled.Pipeline_PullRequestTriggerData{
			SourceBranch: pull.SourceBranch,
			TargetBranch: pull.TargetBranch,
			SourceSha:    sha,
			Pull:         pullAt,
		}
	} else {
		var refPtr *string
		if ref != "" {
			refPtr = &ref
		}
		trigger.Kind = string(workflow.TriggerKindManual)
		trigger.Manual = &tangled.Pipeline_ManualTriggerData{
			Sha:    sha,
			Ref:    refPtr,
			Inputs: inputs,
		}
	}

	repoCloneUri, repoPath, sourceInfo, err := s.resolveCheckout(ctx, repoDid, sourceRepo)
	if err != nil {
		return "", err
	}
	if sourceInfo == nil {
		sourceInfo = triggerRepo
	} else {
		sourceRepoStr := sourceRepo.String()
		trigger.SourceRepo = &sourceRepoStr
	}

	pipelineId, err := s.runPipeline(ctx, repoDid, trigger, nil, repoCloneUri, repoPath, sha, workflows, sourceInfo)
	if err != nil {
		return "", err
	}
	if pipelineId.Rkey == "" {
		return "", xrpc.ErrNoMatchingWorkflows
	}
	return pipelineId.AtUri(), nil
}

// sourceInfo is nil when the checkout comes from the target repo.
func (s *Spindle) resolveCheckout(ctx context.Context, repoDid syntax.DID, sourceRepo syntax.DID) (cloneUri, repoPath string, sourceInfo *tangled.Pipeline_TriggerRepo, err error) {
	repo, err := s.db.GetRepoByDid(repoDid)
	if err != nil {
		return "", "", nil, fmt.Errorf("unknown repoDid %s: %w", repoDid, err)
	}

	cloneUri = s.newRepoCloneUrl(repo.Knot, repoDid)
	repoPath = s.newRepoPath(repoDid)
	if sourceRepo != "" && sourceRepo != repoDid {
		sourceInfo, err = s.resolveSourceRepoInfo(ctx, sourceRepo)
		if err != nil {
			return "", "", nil, err
		}
		cloneUri = models.BuildRepoURL(sourceInfo)
		repoPath = s.newRepoPath(sourceRepo)
	}
	return cloneUri, repoPath, sourceInfo, nil
}

// resolves the workflow definition at sha without executing it
// returns a deterministic fingerprint over the resolved files.
func (s *Spindle) DescribeWorkflowDefinition(ctx context.Context, repoDid syntax.DID, sha string, sourceRepo syntax.DID) (*tangled.CiDescribeWorkflowDefinition_Output, error) {
	repoCloneUri, repoPath, _, err := s.resolveCheckout(ctx, repoDid, sourceRepo)
	if err != nil {
		return nil, err
	}

	rawPipeline, err := s.loadPipeline(ctx, repoCloneUri, repoPath, sha)
	if err != nil {
		return nil, fmt.Errorf("loading pipeline: %w", err)
	}

	hash := fingerprintWorkflowDefinition(rawPipeline)
	workflows := make([]string, 0, len(rawPipeline))
	for _, w := range rawPipeline {
		workflows = append(workflows, w.Name)
	}

	return &tangled.CiDescribeWorkflowDefinition_Output{
		Derived:   true,
		Hash:      &hash,
		Workflows: workflows,
	}, nil
}

func fingerprintWorkflowDefinition(rawPipeline workflow.RawPipeline) string {
	sorted := make([]workflow.RawWorkflow, len(rawPipeline))
	copy(sorted, rawPipeline)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Name < sorted[j].Name })

	h := sha256.New()
	var lenBuf [8]byte
	for _, w := range sorted {
		binary.LittleEndian.PutUint64(lenBuf[:], uint64(len(w.Contents)))
		h.Write([]byte(w.Name))
		// terminate name to avoid ["fo", "o"] == ["f", "oo"]
		h.Write([]byte{0})
		h.Write(lenBuf[:])
		h.Write(w.Contents)
	}
	return fmt.Sprintf("sha256:%x", h.Sum(nil))
}

func (s *Spindle) loadPipeline(ctx context.Context, repoUri, repoPath, rev string) (workflow.RawPipeline, error) {
	if err := git.SparseSyncGitRepo(ctx, repoUri, repoPath, rev); err != nil {
		return nil, fmt.Errorf("syncing git repo: %w", err)
	}
	gr, err := kgit.Open(repoPath, rev)
	if err != nil {
		return nil, fmt.Errorf("opening git repo: %w", err)
	}

	workflowDir, err := gr.FileTree(ctx, workflow.WorkflowDir)
	if errors.Is(err, object.ErrDirectoryNotFound) {
		// return empty RawPipeline when directory doesn't exist
		return nil, nil
	} else if err != nil {
		return nil, fmt.Errorf("loading file tree: %w", err)
	}

	var rawPipeline workflow.RawPipeline
	for _, e := range workflowDir {
		if !e.IsFile() {
			continue
		}

		fpath := filepath.Join(workflow.WorkflowDir, e.Name)
		contents, err := gr.RawContent(fpath)
		if err != nil {
			return nil, fmt.Errorf("reading raw content of '%s': %w", fpath, err)
		}

		rawPipeline = append(rawPipeline, workflow.RawWorkflow{
			Name:     e.Name,
			Contents: contents,
		})
	}

	return rawPipeline, nil
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

func (s *Spindle) StartJobWorkers(ctx context.Context) {
	for range s.cfg.Server.MaxJobCount {
		go func() {
			for {
				job, err := s.db.DequeueJob(ctx)
				if err != nil {
					s.l.Error("failed to dequeue job", "error", err)
				}
				if job == nil {
					// sleep until a new job wakes us
					select {
					case <-ctx.Done():
						return
					case <-s.jobWake:
					}
					continue
				}
				s.runJob(ctx, job)
			}
		}()
	}
}

func (s *Spindle) runJob(ctx context.Context, job *db.JobRow) {
	pipelineId := models.PipelineId{
		Knot: job.PipelineIdKnot,
		Rkey: job.PipelineIdRkey,
	}

	pipelineEnv := models.PipelineEnvVarsForSource(job.Tpl.TriggerMetadata, pipelineId, job.SourceRepo)
	trustedSource := true
	if tm := job.Tpl.TriggerMetadata; tm != nil && tm.SourceRepo != nil &&
		*tm.SourceRepo != "" && *tm.SourceRepo != job.RepoDid {
		trustedSource = false
	}

	initTpl := job.Tpl
	if job.SourceRepo != nil && job.Tpl.TriggerMetadata != nil {
		tm := *job.Tpl.TriggerMetadata
		tm.Repo = job.SourceRepo
		initTpl.TriggerMetadata = &tm
	}

	workflows := make(map[models.Engine][]models.Workflow)
	for _, w := range job.Tpl.Workflows {
		if w == nil {
			continue
		}
		eng, ok := s.engs[w.Engine]
		if !ok {
			_ = s.db.StatusFailed(models.WorkflowId{
				PipelineId: pipelineId,
				Name:       w.Name,
			}, fmt.Sprintf("unknown engine %#v", w.Engine), -1, s.n)
			continue
		}

		ewf, err := eng.InitWorkflow(*w, initTpl)
		if err != nil {
			_ = s.db.StatusFailed(models.WorkflowId{
				PipelineId: pipelineId,
				Name:       w.Name,
			}, fmt.Sprintf("init workflow: %s", err), -1, s.n)
			continue
		}

		if ewf.Environment == nil {
			ewf.Environment = make(map[string]string)
		}
		maps.Copy(ewf.Environment, pipelineEnv)
		workflows[eng] = append(workflows[eng], *ewf)
	}

	engine.StartWorkflows(log.SubLogger(s.l, "engine"), s.vault, s.cfg, s.stores, s.db, s.n, s.rootCtx, &models.Pipeline{
		RepoDid:       syntax.DID(job.RepoDid),
		Workflows:     workflows,
		TrustedSource: trustedSource,
	}, pipelineId)
}

// enqueues the workflows in tpl.
func (s *Spindle) processPipeline(repoDid syntax.DID, tpl tangled.Pipeline, pipelineId models.PipelineId, sourceRepo *tangled.Pipeline_TriggerRepo) error {
	err := s.db.EnqueueJob(s.rootCtx, repoDid.String(), pipelineId, sourceRepo, tpl)
	if err != nil {
		return fmt.Errorf("failed to enqueue durable job: %w", err)
	}
	s.l.Info("pipeline enqueued successfully to db", "id", pipelineId)

	// wake up an idle worker to pick up more jobs if any
	select {
	case s.jobWake <- struct{}{}:
	default:
	}

	// pipelines visible from now on, they are sitting in queue
	for _, w := range tpl.Workflows {
		if w == nil {
			continue
		}
		if err := s.db.StatusPending(models.WorkflowId{
			PipelineId: pipelineId,
			Name:       w.Name,
		}, s.n); err != nil {
			return fmt.Errorf("db.StatusPending: %w", err)
		}
	}
	return nil
}
