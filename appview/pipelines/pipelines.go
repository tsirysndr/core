package pipelines

import (
	"bytes"
	"context"
	"log/slog"
	"net/http"
	"sync"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/hostutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/lexutil"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
	"github.com/gorilla/websocket"
)

type Pipelines struct {
	repoResolver *reporesolver.RepoResolver
	idResolver   *idresolver.Resolver
	config       *config.Config
	oauth        *oauth.OAuth
	pages        *pages.Pages
	db           *db.DB
	enforcer     *rbac.Enforcer
	logger       *slog.Logger
}

func (p *Pipelines) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()
	r.Get("/", p.Index)
	r.Get("/{pipeline}/workflow/{workflow}", p.Workflow)
	r.Get("/{pipeline}/workflow/{workflow}/logs", p.Logs)
	r.
		With(mw.RepoPermissionMiddleware("repo:owner")).
		Post("/{pipeline}/workflow/{workflow}/cancel", p.CancelWorkflow)

	return r
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	pages *pages.Pages,
	idResolver *idresolver.Resolver,
	db *db.DB,
	config *config.Config,
	enforcer *rbac.Enforcer,
	logger *slog.Logger,
) *Pipelines {
	return &Pipelines{
		oauth:        oauth,
		repoResolver: repoResolver,
		pages:        pages,
		idResolver:   idResolver,
		config:       config,
		db:           db,
		enforcer:     enforcer,
		logger:       logger,
	}
}

func (p *Pipelines) Index(w http.ResponseWriter, r *http.Request) {
	user := p.oauth.GetMultiAccountUser(r)
	l := p.logger.With("handler", "Index")

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	filterKind := r.URL.Query().Get("trigger")
	filters := []orm.Filter{
		orm.FilterEq("p.repo_did", f.RepoDid),
	}
	switch filterKind {
	case "push":
		filters = append(filters, orm.FilterEq("t.kind", "push"))
	case "pull_request":
		filters = append(filters, orm.FilterEq("t.kind", "pull_request"))
	default:
		// no filters otherwise, default to "all"
		filterKind = "all"
	}

	if f.Spindle == "" {
		p.pages.Pipelines(w, pages.PipelinesParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
			RepoInfo:   p.repoResolver.GetRepoInfo(r, user),
			Pipelines:  nil,
			FilterKind: filterKind,
			Total:      0,
		})
		return
	}

	spindleUrl, err := hostutil.EnsureHttpScheme(f.Spindle)
	if err != nil {
		l.Error("invalid spindle host", "host", f.Spindle, "err", err)
		p.pages.Pipelines(w, pages.PipelinesParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
			RepoInfo:   p.repoResolver.GetRepoInfo(r, user),
			Pipelines:  nil,
			FilterKind: filterKind,
			Total:      0,
		})
		return
	}

	// sh.tangled.ci.queryPipelines(repo, kind, limit=30)
	xrpcc := indigoxrpc.Client{Host: spindleUrl}
	out, err := tangled.CiQueryPipelines(r.Context(), &xrpcc, nil, "", 30, f.RepoDid)
	if err != nil {
		l.Error("failed to fetch pipelines", "err", err)
		p.pages.Pipelines(w, pages.PipelinesParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
			RepoInfo:   p.repoResolver.GetRepoInfo(r, user),
			Pipelines:  nil,
			FilterKind: filterKind,
			Total:      0,
		})
		return
	}

	var pipelines []types.Pipeline
	for _, pipeline := range out.Pipelines {
		pipelines = append(pipelines, types.Pipeline{CiDefs_Pipeline: pipeline})
	}

	p.pages.Pipelines(w, pages.PipelinesParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   p.repoResolver.GetRepoInfo(r, user),
		Pipelines:  pipelines,
		FilterKind: filterKind,
		Total:      out.Total,
	})
}

func (p *Pipelines) Workflow(w http.ResponseWriter, r *http.Request) {
	user := p.oauth.GetMultiAccountUser(r)
	l := p.logger.With("handler", "Workflow")

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		p.pages.Error404(w)
		return
	}

	pipelineId, err := syntax.ParseTID(chi.URLParam(r, "pipeline"))
	if err != nil {
		l.Debug("invalid pipeline id", "id", pipelineId)
		p.pages.Error404(w)
		return
	}

	workflowName := chi.URLParam(r, "workflow")
	if workflowName == "" {
		l.Debug("empty workflow name")
		p.pages.Error404(w)
		return
	}

	l = l.With("pipeline", pipelineId, "workflow", workflowName)

	// TODO: change url path to:
	// /{owner}/{slug}/pipelines/{spindle-did}/{pipeline-id}/workflow/{workflow-id}

	if f.Spindle == "" {
		p.pages.Error404(w)
		return
	}

	spindleUrl, err := hostutil.EnsureHttpScheme(f.Spindle)
	if err != nil {
		l.Error("invalid spindle host", "host", f.Spindle, "err", err)
		p.pages.Error404(w)
		return
	}

	xrpcc := &indigoxrpc.Client{Host: spindleUrl}
	out, err := tangled.CiGetPipeline(r.Context(), xrpcc, pipelineId.String())
	if err != nil {
		// TODO(boltless): change behavior based on error
		l.Debug("failed to get pipeline", "err", err)
		p.pages.Error404(w)
		return
	}

	// ensure workflow exists
	exist := false
	for _, workflow := range out.Workflows {
		if workflow.Name == workflowName {
			exist = true
			break
		}
	}
	if !exist {
		l.Debug("workflow doesn't exist in pipeline")
		p.pages.Error404(w)
		return
	}

	p.pages.Workflow(w, pages.WorkflowParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   p.repoResolver.GetRepoInfo(r, user),
		Pipeline:   types.Pipeline{CiDefs_Pipeline: out},
		Workflow:   workflowName,
	})
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

type webLogScheduler struct {
	ch chan *tangled.CiPipelineSubscribeLogs_Event
}

var _ lexutil.Scheduler[tangled.CiPipelineSubscribeLogs_Event] = (*webLogScheduler)(nil)

// AddWork implements [lexutil.Scheduler].
func (w *webLogScheduler) AddWork(ctx context.Context, _ string, val *tangled.CiPipelineSubscribeLogs_Event) error {
	select {
	case w.ch <- val:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Shutdown implements [lexutil.Scheduler].
func (w *webLogScheduler) Shutdown() { close(w.ch) }

func (p *Pipelines) Logs(w http.ResponseWriter, r *http.Request) {
	l := p.logger.With("handler", "logs")

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		http.Error(w, "bad repo/knot", http.StatusBadRequest)
		return
	}

	if f.Spindle == "" {
		http.Error(w, "invalid repo info", http.StatusBadRequest)
		return
	}

	pipelineId, err := syntax.ParseTID(chi.URLParam(r, "pipeline"))
	if err != nil {
		l.Debug("invalid pipeline id", "id", pipelineId)
		http.Error(w, "invalid pipeline id", http.StatusBadRequest)
		return
	}

	workflowName := chi.URLParam(r, "workflow")
	if workflowName == "" {
		l.Debug("empty workflow name")
		http.Error(w, "invalid workflow name", http.StatusBadRequest)
		return
	}

	clientConn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		l.Error("websocket upgrade failed", "err", err)
		return
	}
	defer clientConn.Close()

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	spindleUrl, err := hostutil.EnsureHttpScheme(f.Spindle)
	if err != nil {
		l.Error("invalid spindle host", "host", f.Spindle, "err", err)
		return
	}

	evChan := make(chan *tangled.CiPipelineSubscribeLogs_Event, 100)
	done := make(chan error, 1)
	sched := &webLogScheduler{ch: evChan}
	xrpcc := &lexutil.Client{Client: indigoxrpc.Client{Host: spindleUrl}}
	go func() {
		done <- tangled.CiPipelineSubscribeLogs(ctx, xrpcc, pipelineId.String(), []string{workflowName}, sched)
	}()

	var lastWriteLk sync.Mutex
	lastWrite := time.Now()

	// Start a goroutine to ping the client periodically to check if it's still
	// alive. If the client doesn't respond to a ping within 5 seconds, we'll
	// close the connection and teardown the consumer.
	go func() {
		ticker := time.NewTicker(30 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				lastWriteLk.Lock()
				lw := lastWrite
				lastWriteLk.Unlock()
				if time.Since(lw) < 30*time.Second {
					continue
				}
				if err := clientConn.WriteControl(websocket.PingMessage, nil, time.Now().Add(5*time.Second)); err != nil {
					l.Warn("failed to ping client", "err", err)
					cancel()
					return
				}
			case <-ctx.Done():
				return
			}
		}
	}()

	clientConn.SetPingHandler(func(message string) error {
		err := clientConn.WriteControl(websocket.PongMessage, []byte(message), time.Now().Add(60*time.Second))
		if err == websocket.ErrCloseSent {
			return nil
		}
		return err
	})

	// Start a goroutine to read messages from the client and discard them.
	go func() {
		for {
			if _, _, err := clientConn.ReadMessage(); err != nil {
				cancel()
				return
			}
		}
	}()

	// Main loop: sole writer of data frames to the client.
	stepStartTimes := make(map[int]time.Time)
	stepAnsi := make(map[int]*ansiState)
	var fragment bytes.Buffer
	for {
		select {
		case <-ctx.Done():
			l.Info("client disconnected")
			return

		case ev, ok := <-evChan:
			if !ok {
				// Stream ended: Shutdown closed the upstream channel.
				if err := <-done; !isExpectedClose(err) {
					l.Error("spindle stream error", "err", err)
				}
				msg := websocket.FormatCloseMessage(websocket.CloseNormalClosure, "finished")
				_ = clientConn.WriteMessage(websocket.CloseMessage, msg)
				return
			}

			fragment.Reset()

			switch {
			case ev.Error != nil:
				l.Error("spindle error frame", "err", ev.Error.Error, "msg", ev.Error.Message)
				return

			case ev.Control != nil:
				c := ev.Control
				step := int(c.Step)
				switch derefStr(c.Status) {
				case "start":
					t := parseRFC3339(c.Time)
					stepStartTimes[step] = t
					// "system" steps are injected by the CI runner; collapse them.
					collapsed := derefStr(c.Kind) == "system"
					err = p.pages.LogBlock(&fragment, pages.LogBlockParams{
						Id:        step,
						Name:      c.Content,
						Command:   derefStr(c.Command),
						Collapsed: collapsed,
						StartTime: t,
					})
				case "end":
					err = p.pages.LogBlockEnd(&fragment, pages.LogBlockEndParams{
						Id:        step,
						StartTime: stepStartTimes[step],
						EndTime:   parseRFC3339(c.Time),
					})
				}

			case ev.Data != nil:
				d := ev.Data
				step := int(d.Step)
				ansi, ok := stepAnsi[step]
				if !ok {
					ansi = NewAnsiState()
					stepAnsi[step] = ansi
				}
				err = p.pages.LogLine(&fragment, pages.LogLineParams{
					Id:      step,
					Content: ansi.Render(d.Content),
				})
			}
			if err != nil {
				l.Error("failed to render log line", "err", err)
				return
			}

			if err = clientConn.WriteMessage(websocket.TextMessage, fragment.Bytes()); err != nil {
				l.Error("error writing to client", "err", err)
				return
			}
			lastWriteLk.Lock()
			lastWrite = time.Now()
			lastWriteLk.Unlock()
		}
	}
}

func (p *Pipelines) CancelWorkflow(w http.ResponseWriter, r *http.Request) {
	l := p.logger.With("handler", "CancelWorkflow")
	errorId := "workflow-error"

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}
	l = l.With("repo", f.RepoDid)

	if f.Spindle == "" {
		l.Debug("spindle is empty")
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}

	pipelineId, err := syntax.ParseTID(chi.URLParam(r, "pipeline"))
	if err != nil {
		l.Debug("invalid pipeline id", "id", pipelineId)
		p.pages.Error404(w)
		return
	}

	workflowName := chi.URLParam(r, "workflow")
	if workflowName == "" {
		l.Debug("empty workflow name")
		p.pages.Error404(w)
		return
	}

	l = l.With("pipeline", pipelineId, "workflow", workflowName)

	hostname, noTLS, err := hostutil.ParseHostname(f.Spindle)
	if err != nil {
		http.Error(w, "invalid spindle hostname", http.StatusBadRequest)
		return
	}

	spindleClient, err := p.oauth.ServiceClient(
		r,
		oauth.WithService(hostname),
		oauth.WithLxm(tangled.PipelineCancelPipelineNSID),
		oauth.WithDev(noTLS),
		oauth.WithTimeout(time.Second*30), // workflow cleanup usually takes time
	)

	if err := tangled.PipelineCancelPipeline(
		r.Context(),
		spindleClient,
		&tangled.PipelineCancelPipeline_Input{
			Repo:     string(f.RepoAt()),
			Pipeline: pipelineId.String(),
			Workflow: workflowName,
		},
	); err != nil {
		l.Error("failed to cancel workflow", "err", err)
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}
	l.Debug("canceled workflow")
}
