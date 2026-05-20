package pipelines

import (
	"strings"

	"github.com/gorilla/websocket"
)

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
