package ssh

import (
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/gorilla/websocket"
	"tangled.org/core/appview/pipelines"
	spindlemodel "tangled.org/core/spindle/models"
)

type step struct {
	id        int
	name      string
	command   string
	kind      spindlemodel.StepKind
	lines     []string
	startTime time.Time
	endTime   time.Time
	finished  bool
}

type logDoneMsg struct {
	workflow string
	err      error
}

type logEventMsg struct {
	workflow string
	ev       pipelines.LogEvent
	conn     *websocket.Conn
	ch       chan pipelines.LogEvent
}

func readNextCmd(workflow string, conn *websocket.Conn, ch chan pipelines.LogEvent) tea.Cmd {
	return func() tea.Msg {
		return readNextLogEvent(workflow, conn, ch)
	}
}

func readNextLogEvent(workflow string, conn *websocket.Conn, ch chan pipelines.LogEvent) tea.Msg {
	ev, ok := <-ch
	if !ok {
		return logDoneMsg{workflow: workflow}
	}
	return logEventMsg{workflow: workflow, ev: ev, conn: conn, ch: ch}
}
