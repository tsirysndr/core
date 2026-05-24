package pipelines

import (
	"html/template"
	"regexp"
	"strings"

	terminal "github.com/buildkite/terminal-to-html/v3"
	"github.com/gorilla/websocket"
	"tangled.org/core/appview/pages/markup"
)

// matches any ANSI escape sequence: ESC [ <params> m
var sequenceRe = regexp.MustCompile(`\x1b\[([\d;]*)m`)

// ansiState tracks the active stack across log lines
// each non-reset SGR code is pushed onto the stack; a reset clears it.
//
// the stack contents are prepended to each new line so colours carry over.
type ansiState struct {
	stack     []string
	sanitizer markup.Sanitizer
}

func NewAnsiState() *ansiState {
	return &ansiState{
		stack:     []string{},
		sanitizer: markup.NewSanitizer(),
	}
}

func (a *ansiState) Render(line string) template.HTML {
	// prepend whatever sequences are still open from the previous line
	prefix := strings.Join(a.stack, "")
	// render current line with the existing prefix
	rendered := terminal.Render([]byte(prefix + line))
	// sanitize
	sanitized := a.sanitizer.SanitizeLogs(rendered)

	// update the stack with sequences from current line
	for _, m := range sequenceRe.FindAllStringSubmatch(line, -1) {
		params := m[1]
		if params == "" || params == "0" || params == "00" {
			a.stack = a.stack[:0]
		} else {
			a.stack = append(a.stack, m[0])
		}
	}

	return template.HTML(sanitized)
}

type LogEvent struct {
	Msg []byte
	Err error
}

func (ev *LogEvent) IsCloseError() bool {
	return websocket.IsCloseError(
		ev.Err,
		websocket.CloseNormalClosure,
		websocket.CloseGoingAway,
		websocket.CloseAbnormalClosure,
	)
}

func ReadLogs(conn *websocket.Conn, ch chan LogEvent) {
	defer close(ch)
	for {
		if conn == nil {
			return
		}
		_, msg, err := conn.ReadMessage()
		if err != nil {
			ch <- LogEvent{Err: err}
			return
		}
		ch <- LogEvent{Msg: msg}
	}
}

func SpindleURL(dev bool, spindle, knot, rkey, workflow string) string {
	scheme := "wss"
	if dev {
		scheme = "ws"
	}
	return scheme + "://" + strings.Join([]string{spindle, "logs", knot, rkey, workflow}, "/")
}
