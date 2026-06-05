package ssh

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/spinner"
	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/gorilla/websocket"
	"tangled.org/core/api/tangled"
	extlexutil "tangled.org/core/lexutil"
)

var (
	colorFg          lipgloss.NoColor   = lipgloss.NoColor{}
	colorBlue        lipgloss.ANSIColor = 4
	colorBrightBlack lipgloss.ANSIColor = 8
)

type tickMsg time.Time

type statusUpdateMsg struct {
	pipeline *tangled.CiDefs_Pipeline
}

type statusUpdateErrMsg struct{ err error }

type pipelineModel struct {
	renderer *lipgloss.Renderer
	xrpcc    *extlexutil.Client
	pipeline *tangled.CiDefs_Pipeline
	selected int
	logs     map[string]*workflowLogs

	// pipeline log stream: cancel tears down the consumer goroutine on quit.
	// the event/done channels are threaded through log messages, not stored here.
	cancel     context.CancelFunc
	streamDone bool
	streamErr  error

	spinner spinner.Model
	width   int
	height  int
}

type workflowLogs struct {
	steps     []step
	stepIndex map[int64]int // stepId -> index map
	vp        viewport.Model
	ready     bool
}

func newPipelineModel(renderer *lipgloss.Renderer, xrpcc *extlexutil.Client, pipeline *tangled.CiDefs_Pipeline, width, height int) *pipelineModel {
	logs := make(map[string]*workflowLogs, len(pipeline.Workflows))
	for _, wf := range pipeline.Workflows {
		logs[wf.Name] = &workflowLogs{stepIndex: make(map[int64]int)}
	}
	sp := spinner.New(spinner.WithSpinner(spinner.Line))
	return &pipelineModel{
		renderer: renderer,
		xrpcc:    xrpcc,
		pipeline: pipeline,
		logs:     logs,
		spinner:  sp,
		width:    width,
		height:   height,
	}
}

func (m *pipelineModel) Init() tea.Cmd {
	return tea.Batch(tick(), m.spinner.Tick, m.subscribeCmd())
}

func tick() tea.Cmd {
	return tea.Tick(time.Second, func(t time.Time) tea.Msg { return tickMsg(t) })
}

// subscribeCmd opens the ci.pipeline.subscribeLogs stream for current pipeline.
// A consumer goroutine pushes decoded events onto the scheduler channel; the
// returned command yields the first event into the bubbletea loop.
func (m *pipelineModel) subscribeCmd() tea.Cmd {
	// cancel existing subscriptions just in case
	if m.cancel != nil {
		m.cancel()
	}
	sched := newEventScheduler()
	done := make(chan error, 1)
	ctx, cancel := context.WithCancel(context.Background())
	m.cancel = cancel

	pipelineId := m.pipeline.Id
	go func() {
		err := tangled.CiPipelineSubscribeLogs(ctx, m.xrpcc, pipelineId, nil, sched)
		done <- err
	}()

	return readEventCmd(sched.ch, done)
}

func readEventCmd(events chan *tangled.CiPipelineSubscribeLogs_Event, done chan error) tea.Cmd {
	return func() tea.Msg {
		ev, ok := <-events
		if !ok {
			return logDoneMsg{err: <-done}
		}
		return logEventMsg{ev: ev, events: events, done: done}
	}
}

// fetchStatusCmd re-fetches the pipeline
func (m *pipelineModel) fetchStatusCmd() tea.Cmd {
	pipelineId := m.pipeline.Id
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		out, err := tangled.CiGetPipeline(ctx, m.xrpcc, pipelineId)
		if err != nil {
			return statusUpdateErrMsg{err: fmt.Errorf("refreshing pipeline: %w", err)}
		}
		return statusUpdateMsg{pipeline: out}
	}
}

func (m *pipelineModel) vpHeight() int {
	return max(m.height-2, 1) // topbar + empty line take 2 lines
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
		// re-render running workflows so elapsed times advance
		m.refreshRunning()
		return m, tick()

	case spinner.TickMsg:
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(msg)
		return m, cmd

	case tea.KeyMsg:
		switch msg.String() {
		case "q", "ctrl+c":
			if m.cancel != nil {
				m.cancel()
			}
			return m, tea.Quit
		case "tab", "right", "l":
			m.selected = (m.selected + 1) % len(m.pipeline.Workflows)
			return m, nil
		case "shift+tab", "left", "h":
			m.selected = (m.selected - 1 + len(m.pipeline.Workflows)) % len(m.pipeline.Workflows)
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
		m.applyEvent(msg.ev)
		return m, readEventCmd(msg.events, msg.done)

	case logDoneMsg:
		m.streamDone = true
		if !isExpectedClose(msg.err) {
			m.streamErr = msg.err
		}
		m.refreshAll()
		// resolve final workflow statuses once now that the stream has ended
		return m, m.fetchStatusCmd()

	case statusUpdateMsg:
		m.pipeline = msg.pipeline
		known := make(map[string]bool, len(m.pipeline.Workflows))
		for _, wf := range m.pipeline.Workflows {
			known[wf.Name] = true
		}
		for name := range m.logs {
			if !known[name] {
				delete(m.logs, name)
			}
		}
		if m.selected >= len(m.pipeline.Workflows) {
			m.selected = max(len(m.pipeline.Workflows)-1, 0)
		}
		m.refreshAll()

	case statusUpdateErrMsg:
		// best-effort final status refresh; ignore failures
	}

	return m, nil
}

func (m *pipelineModel) selectedLogs() *workflowLogs {
	if len(m.pipeline.Workflows) < 1+m.selected {
		return nil
	}
	return m.logs[m.pipeline.Workflows[m.selected].Name]
}

func (m *pipelineModel) initViewport(wl *workflowLogs) {
	if wl.ready {
		return
	}
	wl.vp = viewport.New(m.width, m.vpHeight())
	wl.ready = true
}

// ensureWorkflow returns the log state for a workflow, lazily creating its routing entry.
func (m *pipelineModel) ensureWorkflow(name string) *workflowLogs {
	wl, ok := m.logs[name]
	if !ok {
		wl = &workflowLogs{stepIndex: make(map[int64]int)}
		m.logs[name] = wl
	}
	return wl
}

// renderWorkflow re-renders a workflow's viewport, preserving bottom-stickiness.
func (m *pipelineModel) renderWorkflow(wl *workflowLogs) {
	m.initViewport(wl)
	atBottom := wl.vp.AtBottom()
	wl.vp.SetContent(renderLogs(m.renderer, wl, m.width))
	if atBottom {
		wl.vp.GotoBottom()
	}
}

// refreshAll re-renders every initialized viewport.
func (m *pipelineModel) refreshAll() {
	for _, wl := range m.logs {
		m.renderWorkflow(wl)
	}
}

// refreshRunning re-renders workflows with unfinished steps so elapsed times advance.
func (m *pipelineModel) refreshRunning() {
	if m.streamDone {
		return
	}
	for _, wl := range m.logs {
		if !wl.ready {
			continue
		}
		for i := range wl.steps {
			if !wl.steps[i].finished {
				m.renderWorkflow(wl)
				break
			}
		}
	}
}

// applyEvent routes a decoded subscribeLogs event into the matching workflow.
func (m *pipelineModel) applyEvent(ev *tangled.CiPipelineSubscribeLogs_Event) {
	switch {
	case ev.Error != nil:
		if ev.Error.Message != "" {
			m.streamErr = fmt.Errorf("%s: %s", ev.Error.Error, ev.Error.Message)
		} else {
			m.streamErr = fmt.Errorf("%s", ev.Error.Error)
		}

	case ev.Control != nil:
		c := ev.Control
		wl := m.ensureWorkflow(c.Workflow)
		switch derefStr(c.Status) {
		case "start":
			wl.stepIndex[c.Step] = len(wl.steps)
			wl.steps = append(wl.steps, step{
				id: c.Step, name: c.Content, command: derefStr(c.Command), startTime: parseRFC3339(c.Time),
			})
		case "end":
			if idx, ok := wl.stepIndex[c.Step]; ok {
				wl.steps[idx].endTime, wl.steps[idx].finished = parseRFC3339(c.Time), true
			}
		}
		m.renderWorkflow(wl)

	case ev.Data != nil:
		d := ev.Data
		wl := m.ensureWorkflow(d.Workflow)
		if idx, ok := wl.stepIndex[d.Step]; ok {
			wl.steps[idx].lines = append(wl.steps[idx].lines, d.Content)
		}
		m.renderWorkflow(wl)
	}
}

// isExpectedClose reports whether err is a clean websocket close (or nil).
func isExpectedClose(err error) bool {
	if err == nil {
		return true
	}
	var ce *websocket.CloseError
	if errors.As(err, &ce) {
		switch ce.Code {
		case websocket.CloseNormalClosure, websocket.CloseGoingAway, websocket.CloseAbnormalClosure:
			return true
		}
	}
	return false
}

// renderLogs builds the full log content string for a workflow, used as viewport content.
func renderLogs(r *lipgloss.Renderer, wl *workflowLogs, width int) string {
	headerStyle := r.NewStyle().Foreground(colorFg).Bold(true)
	cmdStyle := r.NewStyle().Foreground(colorBlue).Width(width)
	dimStyle := r.NewStyle().Faint(true)
	var sb strings.Builder
	for i := range wl.steps {
		st := &wl.steps[i]
		dur := ""
		if st.finished {
			dur = st.endTime.Sub(st.startTime).Round(time.Millisecond).String()
		} else if !st.startTime.IsZero() {
			dur = time.Since(st.startTime).Round(time.Second).String()
		}
		// build overlay: "── name ──...── dur ──"
		nameStr := headerStyle.Render(st.name + " ")
		durStr := headerStyle.Render(" " + dur + " ")
		nameW := lipgloss.Width(nameStr)
		durW := lipgloss.Width(durStr)
		fillW := max(width-nameW-durW, 0)
		fill := dimStyle.Render(strings.Repeat("─", fillW))
		header := nameStr + fill + durStr
		sb.WriteString(header + "\n")
		if st.command != "" {
			sb.WriteString(cmdStyle.Render(st.command) + "\n")
		}
		for _, l := range st.lines {
			sb.WriteString(l + "\n")
		}
		sb.WriteString("\n")
	}
	return sb.String()
}

func (m *pipelineModel) View() string {
	body := ""
	if wl := m.selectedLogs(); wl != nil && wl.ready {
		body = wl.vp.View()
	}
	if m.streamErr != nil {
		body = lipgloss.JoinVertical(lipgloss.Left, body, m.renderer.NewStyle().Foreground(colorBlue).Render("stream error: "+m.streamErr.Error()))
	}
	return lipgloss.JoinVertical(lipgloss.Left, m.topbarView(), "", body)
}

// topbarView renders the single-line tab bar with workflow tabs left and trigger info + help right.
func (m *pipelineModel) topbarView() string {
	r := m.renderer
	activeStyle := r.NewStyle().Background(colorBlue).Foreground(colorFg).Bold(true)

	now := time.Now()

	var tabs strings.Builder
	for i, wf := range m.pipeline.Workflows {
		status := wf.Status
		elapsed := workflowElapsed(wf, now).Round(time.Second).String()
		base := " " + statusIcon(status, m.spinner.View()) + " " + wf.Name
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
				dim := r.NewStyle().Faint(true)
				tabs.WriteString(" " + dim.Render(elapsed))
			}
			tabs.WriteString(" ")
		}
	}

	tabsStr := tabs.String()
	infoStr := triggerLine(r, m.pipeline.Trigger, m.pipeline.Commit) + " · " + helpText(r)

	gap := max(m.width-lipgloss.Width(tabsStr)-lipgloss.Width(infoStr), 1)

	return tabsStr + strings.Repeat(" ", gap) + infoStr
}

func helpText(r *lipgloss.Renderer) string {
	key := r.NewStyle().Foreground(colorFg)
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

func triggerLine(r *lipgloss.Renderer, t *tangled.CiDefs_Pipeline_Trigger, sha string) string {
	hash := shortSha(sha)
	dim := r.NewStyle().Faint(true)
	if t == nil {
		return dim.Render(hash)
	}
	if t.CiTrigger_Push != nil {
		return t.CiTrigger_Push.Ref + dim.Render("@"+hash) + dim.Render(" (push)")
	}
	if t.CiTrigger_PullRequest != nil {
		source := ""
		if t.CiTrigger_PullRequest.SourceBranch != nil {
			source = *t.CiTrigger_PullRequest.SourceBranch
		}
		return t.CiTrigger_PullRequest.TargetBranch + dim.Render(" <- "+source+"@"+hash) + dim.Render(" (pull-request)")
	}
	return dim.Render(hash)
}

func statusIcon(status string, spinnerFrame string) string {
	switch status {
	case "success":
		return "✓"
	case "failed":
		return "×"
	case "running":
		return spinnerFrame
	case "pending":
		return "·"
	case "timeout":
		return "⌀"
	case "cancelled":
		return "-"
	default:
		return "?"
	}
}

func derefStr(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

func parseRFC3339(s string) time.Time {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		return time.Time{}
	}
	return t
}
