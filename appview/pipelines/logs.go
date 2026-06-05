package pipelines

import (
	"errors"
	"html/template"
	"regexp"
	"strings"
	"time"

	terminal "github.com/buildkite/terminal-to-html/v3"
	"github.com/gorilla/websocket"
	"tangled.org/core/appview/pages/markup/sanitizer"
)

// matches any ANSI escape sequence: ESC [ <params> m
var sequenceRe = regexp.MustCompile(`\x1b\[([\d;]*)m`)

// ansiState tracks the active stack across log lines
// each non-reset SGR code is pushed onto the stack; a reset clears it.
//
// the stack contents are prepended to each new line so colours carry over.
type ansiState struct {
	stack []string
}

func NewAnsiState() *ansiState {
	return &ansiState{
		stack: []string{},
	}
}

func (a *ansiState) Render(line string) template.HTML {
	// prepend whatever sequences are still open from the previous line
	prefix := strings.Join(a.stack, "")
	// render current line with the existing prefix
	rendered := terminal.Render([]byte(prefix + line))
	// sanitize
	sanitized := sanitizer.SanitizeLogs(rendered)

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
