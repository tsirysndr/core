package pipelines

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	spindlemodel "tangled.org/core/spindle/models"

	"github.com/go-chi/chi/v5"
	"github.com/gorilla/websocket"
)

type Pipelines struct {
	repoResolver     *reporesolver.RepoResolver
	idResolver       *idresolver.Resolver
	config           *config.Config
	oauth            *oauth.OAuth
	pages            *pages.Pages
	spindlestream    *eventconsumer.Consumer
	pipelineNotifier *StatusNotifier
	db               *db.DB
	enforcer         *rbac.Enforcer
	logger           *slog.Logger
}

func (p *Pipelines) Router(mw *middleware.Middleware) http.Handler {
	r := chi.NewRouter()
	r.Get("/", p.Index)
	r.Get("/{pipeline}/workflow/{workflow}", p.Workflow)
	r.Get("/{pipeline}/workflow/{workflow}/logs", p.Logs)
	r.
		With(mw.RepoPermissionMiddleware("repo:owner")).
		Post("/{pipeline}/workflow/{workflow}/cancel", p.Cancel)

	return r
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	pages *pages.Pages,
	spindlestream *eventconsumer.Consumer,
	pipelineNotifier *StatusNotifier,
	idResolver *idresolver.Resolver,
	db *db.DB,
	config *config.Config,
	enforcer *rbac.Enforcer,
	logger *slog.Logger,
) *Pipelines {
	return &Pipelines{
		oauth:            oauth,
		repoResolver:     repoResolver,
		pages:            pages,
		idResolver:       idResolver,
		config:           config,
		spindlestream:    spindlestream,
		pipelineNotifier: pipelineNotifier,
		db:               db,
		enforcer:         enforcer,
		logger:           logger,
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

	ps, err := db.GetPipelineStatuses(
		p.db,
		30,
		filters...,
	)
	if err != nil {
		l.Error("failed to query db", "err", err)
		return
	}

	total, err := db.GetPipelineCount(p.db, filters...)
	if err != nil {
		l.Error("failed to query db", "err", err)
		return
	}

	p.pages.Pipelines(w, pages.PipelinesParams{
		LoggedInUser: user,
		RepoInfo:     p.repoResolver.GetRepoInfo(r, user),
		Pipelines:    ps,
		FilterKind:   filterKind,
		Total:        total,
	})
}

func (p *Pipelines) Workflow(w http.ResponseWriter, r *http.Request) {
	user := p.oauth.GetMultiAccountUser(r)
	l := p.logger.With("handler", "Workflow")

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	pipelineId := chi.URLParam(r, "pipeline")
	if pipelineId == "" {
		l.Error("empty pipeline ID")
		return
	}

	workflow := chi.URLParam(r, "workflow")
	if workflow == "" {
		l.Error("empty workflow name")
		return
	}

	ps, err := db.GetPipelineStatuses(
		p.db,
		1,
		orm.FilterEq("p.repo_did", f.RepoDid),
		orm.FilterEq("p.id", pipelineId),
	)
	if err != nil {
		l.Error("failed to query db", "err", err)
		return
	}

	if len(ps) != 1 {
		l.Error("invalid number of pipelines", "len", len(ps))
		return
	}

	singlePipeline := ps[0]

	p.pages.Workflow(w, pages.WorkflowParams{
		LoggedInUser: user,
		RepoInfo:     p.repoResolver.GetRepoInfo(r, user),
		Pipeline:     singlePipeline,
		Workflow:     workflow,
	})
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 1024,
}

func (p *Pipelines) Logs(w http.ResponseWriter, r *http.Request) {
	l := p.logger.With("handler", "logs")

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		http.Error(w, "bad repo/knot", http.StatusBadRequest)
		return
	}

	pipelineId := chi.URLParam(r, "pipeline")
	workflow := chi.URLParam(r, "workflow")
	if pipelineId == "" || workflow == "" {
		http.Error(w, "missing pipeline ID or workflow", http.StatusBadRequest)
		return
	}

	ps, err := db.GetPipelineStatuses(
		p.db,
		1,
		orm.FilterEq("p.repo_did", f.RepoDid),
		orm.FilterEq("p.id", pipelineId),
	)
	if err != nil || len(ps) != 1 {
		l.Error("pipeline query failed", "err", err, "count", len(ps))
		http.Error(w, "pipeline not found", http.StatusNotFound)
		return
	}

	singlePipeline := ps[0]
	spindle := f.Spindle
	knot := f.Knot
	rkey := singlePipeline.Rkey

	statusCh := p.pipelineNotifier.Subscribe(singlePipeline.AtUri())
	defer p.pipelineNotifier.Unsubscribe(singlePipeline.AtUri(), statusCh)

	if spindle == "" || knot == "" || rkey == "" {
		http.Error(w, "invalid repo info", http.StatusBadRequest)
		return
	}

	url := SpindleURL(p.config.Core.Dev, spindle, knot, rkey, workflow)
	l = l.With("url", url)

	clientConn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		l.Error("websocket upgrade failed", "err", err)
		return
	}
	defer func() {
		_ = clientConn.WriteControl(
			websocket.CloseMessage,
			websocket.FormatCloseMessage(websocket.CloseNormalClosure, "log stream complete"),
			time.Now().Add(time.Second),
		)
		clientConn.Close()
	}()

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	l.Info("logs endpoint hit")

	spindleConn, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		l.Error("websocket dial failed", "err", err)
		return
	}
	defer spindleConn.Close()

	// create a channel for incoming messages
	evChan := make(chan LogEvent, 100)
	// start a goroutine to read from spindle
	go ReadLogs(spindleConn, evChan)

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
				continue
			}

			if ev.Err != nil && ev.IsCloseError() {
				l.Debug("graceful shutdown, tail complete", "err", err)
				return
			}
			if ev.Err != nil {
				l.Error("error reading from spindle", "err", err)
				return
			}

			var logLine spindlemodel.LogLine
			if err = json.Unmarshal(ev.Msg, &logLine); err != nil {
				l.Error("failed to parse logline", "err", err)
				continue
			}

			fragment.Reset()

			switch logLine.Kind {
			case spindlemodel.LogKindControl:
				switch logLine.StepStatus {
				case spindlemodel.StepStatusStart:
					stepStartTimes[logLine.StepId] = logLine.Time
					collapsed := false
					if logLine.StepKind == spindlemodel.StepKindSystem {
						collapsed = true
					}
					err = p.pages.LogBlock(&fragment, pages.LogBlockParams{
						Id:        logLine.StepId,
						Name:      logLine.Content,
						Command:   logLine.StepCommand,
						Collapsed: collapsed,
						StartTime: logLine.Time,
					})
				case spindlemodel.StepStatusEnd:
					startTime := stepStartTimes[logLine.StepId]
					endTime := logLine.Time
					err = p.pages.LogBlockEnd(&fragment, pages.LogBlockEndParams{
						Id:        logLine.StepId,
						StartTime: startTime,
						EndTime:   endTime,
					})
				}

			case spindlemodel.LogKindData:
				ansi, ok := stepAnsi[logLine.StepId]
				if !ok {
					ansi = NewAnsiState()
					stepAnsi[logLine.StepId] = ansi
				}
				err = p.pages.LogLine(&fragment, pages.LogLineParams{
					Id:      logLine.StepId,
					Content: ansi.Render(logLine.Content),
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

		case _, ok := <-statusCh:
			if !ok {
				continue
			}
			fresh, err := db.GetPipelineStatuses(
				p.db,
				1,
				orm.FilterEq("p.repo_did", f.RepoDid),
				orm.FilterEq("p.id", pipelineId),
			)
			if err != nil || len(fresh) == 0 {
				continue
			}
			for name, ws := range fresh[0].Statuses {
				fragment.Reset()
				if err = p.pages.WorkflowSymbolOOB(&fragment, pages.WorkflowSymbolOOBParams{
					Name:     name,
					Statuses: ws,
				}); err != nil {
					l.Error("failed to render workflow symbol OOB", "err", err)
					continue
				}
				if err = clientConn.WriteMessage(websocket.TextMessage, fragment.Bytes()); err != nil {
					l.Error("error writing workflow symbol to client", "err", err)
					return
				}
			}

		case <-time.After(30 * time.Second):
			l.Debug("sent keepalive")
			if err = clientConn.WriteControl(websocket.PingMessage, []byte{}, time.Now().Add(time.Second)); err != nil {
				l.Error("failed to write control", "err", err)
				return
			}
		}
	}
}

func (p *Pipelines) Cancel(w http.ResponseWriter, r *http.Request) {
	l := p.logger.With("handler", "Cancel")

	var (
		pipelineId = chi.URLParam(r, "pipeline")
		workflow   = chi.URLParam(r, "workflow")
	)
	if pipelineId == "" || workflow == "" {
		http.Error(w, "missing pipeline ID or workflow", http.StatusBadRequest)
		return
	}

	f, err := p.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		http.Error(w, "bad repo/knot", http.StatusBadRequest)
		return
	}

	pipeline, err := func() (models.Pipeline, error) {
		ps, err := db.GetPipelineStatuses(
			p.db,
			1,
			orm.FilterEq("p.repo_did", f.RepoDid),
			orm.FilterEq("p.id", pipelineId),
		)
		if err != nil {
			return models.Pipeline{}, err
		}
		if len(ps) != 1 {
			return models.Pipeline{}, fmt.Errorf("wrong pipeline count %d", len(ps))
		}
		return ps[0], nil
	}()
	if err != nil {
		l.Error("pipeline query failed", "err", err)
		http.Error(w, "pipeline not found", http.StatusNotFound)
	}
	var (
		spindle = f.Spindle
		knot    = f.Knot
		rkey    = pipeline.Rkey
	)

	if spindle == "" || knot == "" || rkey == "" {
		http.Error(w, "invalid repo info", http.StatusBadRequest)
		return
	}

	spindleClient, err := p.oauth.ServiceClient(
		r,
		oauth.WithService(f.Spindle),
		oauth.WithLxm(tangled.PipelineCancelPipelineNSID),
		oauth.WithDev(p.config.Core.Dev),
		oauth.WithTimeout(time.Second*30), // workflow cleanup usually takes time
	)

	err = tangled.PipelineCancelPipeline(
		r.Context(),
		spindleClient,
		&tangled.PipelineCancelPipeline_Input{
			Repo:     string(f.RepoAt()),
			Pipeline: pipeline.AtUri().String(),
			Workflow: workflow,
		},
	)
	errorId := "workflow-error"
	if err != nil {
		l.Error("failed to cancel workflow", "err", err)
		p.pages.Notice(w, errorId, "Failed to cancel workflow")
		return
	}
	l.Debug("canceled pipeline", "uri", pipeline.AtUri())
}
