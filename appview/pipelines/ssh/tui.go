package ssh

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/spinner"
	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/gorilla/websocket"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pipelines"
	"tangled.org/core/orm"
	spindlemodel "tangled.org/core/spindle/models"
)

var (
	colorWhite       lipgloss.ANSIColor = 7
	colorBlue        lipgloss.ANSIColor = 4
	colorBrightBlack lipgloss.ANSIColor = 8
	colorDarkGrey    lipgloss.ANSIColor = 0
)

type tickMsg time.Time

type statusUpdateMsg struct {
	pipeline models.Pipeline
}

type statusUpdateErrMsg struct{ err error }

type pipelineModel struct {
	renderer  *lipgloss.Renderer
	server    *Server
	pipeline  models.Pipeline
	workflows []string
	selected  int
	logs      map[string]*workflowLogs
	statusCh  chan struct{}
	spinner   spinner.Model
	width     int
	height    int
}

type workflowLogs struct {
	steps     []step
	stepIndex map[int]int
	vp        viewport.Model
	ready     bool
	done      bool
	err       error
}

func newPipelineModel(renderer *lipgloss.Renderer, s *Server, pipeline models.Pipeline, width, height int) *pipelineModel {
	workflows := pipeline.Workflows()
	logs := make(map[string]*workflowLogs, len(workflows))
	for _, wf := range workflows {
		logs[wf] = &workflowLogs{stepIndex: make(map[int]int)}
	}
	statusCh := s.pipelineNotifier.Subscribe(pipeline.AtUri())
	sp := spinner.New(spinner.WithSpinner(spinner.Line))
	return &pipelineModel{
		renderer:  renderer,
		server:    s,
		pipeline:  pipeline,
		workflows: workflows,
		logs:      logs,
		statusCh:  statusCh,
		spinner:   sp,
		width:     width,
		height:    height,
	}
}

func (m *pipelineModel) Init() tea.Cmd {
	cmds := []tea.Cmd{tick(), m.spinner.Tick, m.waitForStatusUpdate(m.statusCh)}
	for _, wf := range m.workflows {
		cmds = append(cmds, m.connectCmd(wf))
	}
	return tea.Batch(cmds...)
}

func tick() tea.Cmd {
	return tea.Tick(time.Second, func(t time.Time) tea.Msg { return tickMsg(t) })
}

// waitForStatusUpdate blocks on the notifier channel, re-fetches pipeline statuses, and returns the result as a tea.Msg.
func (m *pipelineModel) waitForStatusUpdate(ch chan struct{}) tea.Cmd {
	knot := m.pipeline.Knot
	rkey := m.pipeline.Rkey
	return func() tea.Msg {
		if _, ok := <-ch; !ok {
			return nil
		}
		ps, err := db.GetPipelineStatuses(m.server.db, 1,
			orm.FilterEq("p.knot", knot),
			orm.FilterEq("p.rkey", rkey),
		)
		if err != nil || len(ps) == 0 {
			return statusUpdateErrMsg{err: fmt.Errorf("refreshing pipeline: %w", err)}
		}
		return statusUpdateMsg{pipeline: ps[0]}
	}
}

// connectCmd dials the spindle websocket for the given workflow and starts streaming log events.
func (m *pipelineModel) connectCmd(workflow string) tea.Cmd {
	return func() tea.Msg {
		ws, ok := m.pipeline.Statuses[workflow]
		if !ok || len(ws.Data) == 0 {
			return logDoneMsg{workflow: workflow}
		}
		url := pipelines.SpindleURL(m.server.config.Core.Dev, ws.Data[0].Spindle, m.pipeline.Knot, m.pipeline.Rkey, workflow)
		conn, _, err := websocket.DefaultDialer.Dial(url, nil)
		if err != nil {
			return logDoneMsg{workflow: workflow, err: fmt.Errorf("connecting to spindle: %w", err)}
		}
		ch := make(chan pipelines.LogEvent, 100)
		go pipelines.ReadLogs(conn, ch)
		return readNextLogEvent(workflow, conn, ch)
	}
}

func (m *pipelineModel) vpHeight() int {
	return max(m.height-2, 1) // topbar + divider take 2 lines
}

// resizeViewports updates all viewport dimensions and re-renders their content after a terminal resize.
//
// TODO: can be tedious if we have logs of logs
func (m *pipelineModel) resizeViewports() {
	for _, wl := range m.logs {
		if !wl.ready {
			continue
		}
		atBottom := wl.vp.AtBottom()
		wl.vp.Width = m.width
		wl.vp.Height = m.vpHeight()
		wl.vp.SetContent(renderLogs(m.renderer, wl, m.width))
		if atBottom {
			wl.vp.GotoBottom()
		}
	}
}

func (m *pipelineModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height
		m.resizeViewports()

	case tickMsg:
		return m, tick()

	case spinner.TickMsg:
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(msg)
		return m, cmd

	case tea.KeyMsg:
		switch msg.String() {
		case "q", "ctrl+c":
			m.server.pipelineNotifier.Unsubscribe(m.pipeline.AtUri(), m.statusCh)
			return m, tea.Quit
		case "tab", "right", "l":
			m.selected = (m.selected + 1) % len(m.workflows)
			return m, nil
		case "shift+tab", "left", "h":
			m.selected = (m.selected - 1 + len(m.workflows)) % len(m.workflows)
			return m, nil
		}
		if wl := m.selectedLogs(); wl != nil && wl.ready {
			switch msg.String() {
			case "g":
				wl.vp.GotoTop()
				return m, nil
			case "G":
				wl.vp.GotoBottom()
				return m, nil
			case "ctrl+e":
				wl.vp.ScrollDown(1)
				return m, nil
			case "ctrl+y":
				wl.vp.ScrollUp(1)
				return m, nil
			}
			var cmd tea.Cmd
			wl.vp, cmd = wl.vp.Update(msg)
			return m, cmd
		}

	case logEventMsg:
		return m, m.handleLogEvent(msg)

	case logDoneMsg:
		if wl, ok := m.logs[msg.workflow]; ok {
			wl.done, wl.err = true, msg.err
			m.initViewport(wl)
			wl.vp.SetContent(renderLogs(m.renderer, wl, m.width))
			wl.vp.GotoBottom()
		}

	case statusUpdateMsg:
		// detect any workflows that are new since the last update
		known := make(map[string]bool, len(m.workflows))
		for _, wf := range m.workflows {
			known[wf] = true
		}
		m.pipeline = msg.pipeline
		var newCmds []tea.Cmd
		for _, wf := range msg.pipeline.Workflows() {
			if !known[wf] {
				m.workflows = append(m.workflows, wf)
				m.logs[wf] = &workflowLogs{stepIndex: make(map[int]int)}
				newCmds = append(newCmds, m.connectCmd(wf))
			}
		}
		// re-subscribe for the next update
		newCmds = append(newCmds, m.waitForStatusUpdate(m.statusCh))
		return m, tea.Batch(newCmds...)

	case statusUpdateErrMsg:
		// re-subscribe even on error so we don't stop listening
		return m, m.waitForStatusUpdate(m.statusCh)
	}

	return m, nil
}

func (m *pipelineModel) selectedLogs() *workflowLogs {
	if len(m.workflows) == 0 {
		return nil
	}
	return m.logs[m.workflows[m.selected]]
}

func (m *pipelineModel) initViewport(wl *workflowLogs) {
	if wl.ready {
		return
	}
	wl.vp = viewport.New(m.width, m.vpHeight())
	wl.ready = true
}

// handleLogEvent processes a single log event, updates the step state, and re-renders the viewport.
func (m *pipelineModel) handleLogEvent(msg logEventMsg) tea.Cmd {
	wl, ok := m.logs[msg.workflow]
	if !ok {
		return nil
	}
	if msg.ev.Err != nil {
		wl.done = true
		if !msg.ev.IsCloseError() {
			wl.err = msg.ev.Err
		}
		m.initViewport(wl)
		wl.vp.SetContent(renderLogs(m.renderer, wl, m.width))
		return nil
	}
	var line spindlemodel.LogLine
	if err := json.Unmarshal(msg.ev.Msg, &line); err != nil {
		return readNextCmd(msg.workflow, msg.conn, msg.ch)
	}
	applyLogLine(wl, line)
	m.initViewport(wl)
	atBottom := wl.vp.AtBottom()
	wl.vp.SetContent(renderLogs(m.renderer, wl, m.width))
	if atBottom {
		wl.vp.GotoBottom()
	}
	return readNextCmd(msg.workflow, msg.conn, msg.ch)
}

// applyLogLine mutates wl by appending the log line to the appropriate step.
func applyLogLine(wl *workflowLogs, line spindlemodel.LogLine) {
	switch line.Kind {
	case spindlemodel.LogKindControl:
		switch line.StepStatus {
		case spindlemodel.StepStatusStart:
			idx := len(wl.steps)
			wl.stepIndex[line.StepId] = idx
			wl.steps = append(wl.steps, step{
				id: line.StepId, name: line.Content, command: line.StepCommand,
				kind: line.StepKind, startTime: line.Time,
			})
		case spindlemodel.StepStatusEnd:
			if idx, ok := wl.stepIndex[line.StepId]; ok {
				wl.steps[idx].endTime, wl.steps[idx].finished = line.Time, true
			}
		}
	case spindlemodel.LogKindData:
		if idx, ok := wl.stepIndex[line.StepId]; ok {
			wl.steps[idx].lines = append(wl.steps[idx].lines, line.Content)
		}
	}
}

// renderLogs builds the full log content string for a workflow, used as viewport content.
func renderLogs(r *lipgloss.Renderer, wl *workflowLogs, width int) string {
	headerStyle := r.NewStyle().Foreground(colorWhite).Background(colorBrightBlack).Bold(true)
	cmdStyle := r.NewStyle().Foreground(colorBlue).Width(width)
	now := time.Now()
	var sb strings.Builder
	for i := range wl.steps {
		st := &wl.steps[i]
		dur := ""
		if st.finished {
			dur = st.endTime.Sub(st.startTime).Round(time.Millisecond).String()
		} else if !st.startTime.IsZero() {
			dur = now.Sub(st.startTime).Round(time.Second).String()
		}
		durRendered := headerStyle.Render(dur)
		nameWidth := max(width-lipgloss.Width(dur)-1, 1)
		header := fmt.Sprintf("%-*s ", nameWidth, st.name) + durRendered
		sb.WriteString(headerStyle.Width(width).Render(header) + "\n")
		if st.command != "" {
			sb.WriteString(cmdStyle.Render(st.command) + "\n")
		}
		for _, l := range st.lines {
			sb.WriteString(l + "\n")
		}
		sb.WriteString("\n")
	}
	if wl.done && wl.err != nil {
		sb.WriteString("error: " + wl.err.Error() + "\n")
	}
	return sb.String()
}

func (m *pipelineModel) View() string {
	r := m.renderer
	divider := r.NewStyle().Foreground(colorBrightBlack).Render(strings.Repeat("─", m.width))
	body := ""
	if wl := m.selectedLogs(); wl != nil && wl.ready {
		body = wl.vp.View()
	}
	return lipgloss.JoinVertical(lipgloss.Left, m.topbarView(), divider, body)
}

// topbarView renders the single-line tab bar with workflow tabs left and trigger info + help right.
func (m *pipelineModel) topbarView() string {
	r := m.renderer
	activeStyle := r.NewStyle().Background(colorBlue).Foreground(colorWhite)

	now := time.Now()

	var tabs strings.Builder
	for i, wf := range m.workflows {
		status := spindlemodel.StatusKindPending
		elapsed := ""
		if ws, ok := m.pipeline.Statuses[wf]; ok {
			latest := ws.Latest()
			status = latest.Status
			if t := ws.TimeTaken(); t > 0 {
				elapsed = t.Round(time.Second).String()
			} else {
				elapsed = now.Sub(latest.Created).Round(time.Second).String()
			}
		}
		dim := r.NewStyle().Faint(true)
		base := " " + statusIcon(status, m.spinner.View()) + " " + wf
		if i == m.selected {
			tab := base
			if elapsed != "" {
				tab += " " + elapsed
			}
			tab += " "
			tabs.WriteString(activeStyle.Render(tab))
		} else {
			tabs.WriteString(base)
			if elapsed != "" {
				tabs.WriteString(" " + dim.Render(elapsed))
			}
			tabs.WriteString(" ")
		}
	}

	tabsStr := tabs.String()
	infoStr := triggerLine(r, m.pipeline.Trigger, m.pipeline.Sha) + " · " + helpText(r)

	gap := max(m.width-lipgloss.Width(tabsStr)-lipgloss.Width(infoStr), 1)

	return tabsStr + strings.Repeat(" ", gap) + infoStr
}

func helpText(r *lipgloss.Renderer) string {
	key := r.NewStyle().Foreground(colorWhite)
	action := r.NewStyle().Faint(true)
	sep := action.Render(" · ")

	items := []string{
		key.Render("←/→") + " " + action.Render("switch"),
		key.Render("↑/↓") + " " + action.Render("scroll"),
		key.Render("q") + " " + action.Render("quit"),
	}
	return strings.Join(items, sep)
}

func shortSha(sha string) string {
	if len(sha) >= 8 {
		return sha[:8]
	}
	return sha
}

func triggerLine(r *lipgloss.Renderer, t *models.Trigger, sha string) string {
	hash := shortSha(sha)
	dim := r.NewStyle().Faint(true)
	if t == nil {
		return dim.Render(hash)
	}
	if t.IsPush() {
		return t.TargetRef() + dim.Render("@"+hash) + dim.Render(" (push)")
	}
	if t.IsPullRequest() {
		source := ""
		if t.PRSourceBranch != nil {
			source = *t.PRSourceBranch
		}
		return t.TargetRef() + dim.Render(" <- "+source+"@"+hash) + dim.Render(" (pull-request)")
	}
	return dim.Render(hash)
}

func statusIcon(status spindlemodel.StatusKind, spinnerFrame string) string {
	switch status {
	case spindlemodel.StatusKindSuccess:
		return "✓"
	case spindlemodel.StatusKindFailed:
		return "×"
	case spindlemodel.StatusKindRunning:
		return spinnerFrame
	case spindlemodel.StatusKindPending:
		return "·"
	case spindlemodel.StatusKindTimeout:
		return "⌀"
	case spindlemodel.StatusKindCancelled:
		return "-"
	default:
		return "?"
	}
}
