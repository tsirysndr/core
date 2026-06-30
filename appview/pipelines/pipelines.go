package pipelines

import (
	"bytes"
	"context"
	"fmt"
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
	r.Group(func(r chi.Router) {
		r.Use(mw.RepoPermissionMiddleware("repo:owner"))
		r.Post("/{pipeline}/workflow/{workflow}/cancel", p.CancelWorkflow)
		r.Post("/{pipeline}/retry", p.RetryPipeline)
		r.Post("/{pipeline}/workflow/{workflow}/retry", p.RetryWorkflow)
	})

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
		pipelines = append(pipelines, types.Pipeline{CiPipeline: pipeline})
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
		Pipeline:   types.Pipeline{CiPipeline: out},
		Workflow:   workflowName,
	})
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

type webLogScheduler struct {
	ch chan *tangled.CiSubscribePipelineLogs_Event
}

var _ lexutil.Scheduler[tangled.CiSubscribePipelineLogs_Event] = (*webLogScheduler)(nil)

// AddWork implements [lexutil.Scheduler].
func (w *webLogScheduler) AddWork(ctx context.Context, _ string, val *tangled.CiSubscribePipelineLogs_Event) error {
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

	evChan := make(chan *tangled.CiSubscribePipelineLogs_Event, 100)
	done := make(chan error, 1)
	sched := &webLogScheduler{ch: evChan}
	xrpcc := &lexutil.Client{Client: indigoxrpc.Client{Host: spindleUrl}}
	go func() {
		done <- tangled.CiSubscribePipelineLogs(ctx, xrpcc, pipelineId.String(), []string{workflowName}, sched)
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

	spindleClient, err := p.spindleServiceClient(r, f.Spindle, tangled.CiPipelineCancelPipelineNSID)
	if err != nil {
		l.Error("failed to prepare spindle client", "err", err)
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}

	pipelineAtUri := fmt.Sprintf("at://did:web:%s/%s/%s", f.Knot, tangled.PipelineNSID, pipelineId.String())
	if err := tangled.CiPipelineCancelPipeline(
		r.Context(),
		spindleClient,
		&tangled.CiPipelineCancelPipeline_Input{
			Repo:      string(f.RepoAt()),
			Pipeline:  pipelineAtUri,
			Workflows: []string{workflowName},
		},
	); err != nil {
		l.Error("failed to cancel workflow", "err", err)
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}
	l.Debug("canceled workflow")
}

// RetryPipeline retries all workflows in a pipeline
func (p *Pipelines) RetryPipeline(w http.ResponseWriter, r *http.Request) {
	p.retry(w, r, "")
}

// RetryWorkflow retries a single workflow in a pipeline
func (p *Pipelines) RetryWorkflow(w http.ResponseWriter, r *http.Request) {
	p.retry(w, r, chi.URLParam(r, "workflow"))
}

// retry triggers a new pipeline run for the original commit, either for all or a single workflow
func (p *Pipelines) retry(w http.ResponseWriter, r *http.Request, only string) {
	user := p.oauth.GetMultiAccountUser(r)
	l := p.logger.With("handler", "retry", "only", only)
	errorId := "workflow-error"

	// fail logs the error and shows a notice to the user
	fail := func(msg string, err error) {
		if err != nil {
			l.Error(msg, "err", err)
			p.pages.Notice(w, errorId, fmt.Sprintf("%s: %v", msg, err))
		} else {
			l.Error(msg)
			p.pages.Notice(w, errorId, msg)
		}
	}

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		fail("failed to resolve repository", err)
		return
	}
	l = l.With("repo", f.RepoDid)

	if f.Spindle == "" {
		fail("this repository has no spindle configured", nil)
		return
	}

	pipelineId, err := syntax.ParseTID(chi.URLParam(r, "pipeline"))
	if err != nil {
		l.Debug("invalid pipeline id", "id", pipelineId)
		p.pages.Error404(w)
		return
	}
	l = l.With("pipeline", pipelineId)

	spindleUrl, err := hostutil.EnsureHttpScheme(f.Spindle)
	if err != nil {
		fail("invalid spindle host", err)
		return
	}

	// fetch the original pipeline to replay the same commit and workflows
	queryClient := &indigoxrpc.Client{Host: spindleUrl}
	orig, err := tangled.CiGetPipeline(r.Context(), queryClient, pipelineId.String())
	if err != nil {
		fail("failed to load the original pipeline", err)
		return
	}
	if orig.Commit == "" {
		fail("cannot retry: the original pipeline has no commit", nil)
		return
	}

	// figure out which workflows to run and where to redirect
	var workflows []string
	if only != "" {
		workflows = []string{only}
	} else {
		for _, wf := range orig.Workflows {
			workflows = append(workflows, wf.Name)
		}
	}
	if len(workflows) == 0 {
		fail("cannot retry: the original pipeline has no workflows", nil)
		return
	}
	redirectWf := workflows[0]

	spindleClient, err := p.spindleServiceClient(r, f.Spindle, tangled.CiTriggerPipelineNSID)
	if err != nil {
		fail("failed to authorize with spindle", err)
		return
	}

	out, err := tangled.CiTriggerPipeline(
		r.Context(),
		spindleClient,
		&tangled.CiTriggerPipeline_Input{
			Repo:      string(f.RepoAt()),
			Sha:       orig.Commit,
			Workflows: workflows,
		},
	)
	if err != nil {
		fail("spindle rejected the trigger", err)
		return
	}

	newAt, err := syntax.ParseATURI(out.Pipeline)
	if err != nil {
		fail("pipeline triggered, but the response was malformed", err)
		return
	}
	newId := newAt.RecordKey().String()
	l = l.With("new", newId)
	l.Info("pipeline retried")

	repoInfo := p.repoResolver.GetRepoInfo(r, user)
	dest := fmt.Sprintf("/%s/pipelines/%s/workflow/%s", repoInfo.FullName(), newId, redirectWf)

	if r.Header.Get("HX-Request") == "true" {
		w.Header().Set("HX-Redirect", dest)
		w.WriteHeader(http.StatusOK)
		return
	}
	http.Redirect(w, r, dest, http.StatusSeeOther)
}

// spindleServiceClient builds an authed spindle xrpc client
func (p *Pipelines) spindleServiceClient(r *http.Request, spindle, lxm string) (*indigoxrpc.Client, error) {
	hostname, noTLS, err := hostutil.ParseHostname(spindle)
	if err != nil {
		return nil, err
	}
	return p.oauth.ServiceClient(
		r,
		oauth.WithService(hostname),
		oauth.WithLxm(lxm),
		oauth.WithDev(noTLS),
		oauth.WithTimeout(time.Second*30),
	)
}
