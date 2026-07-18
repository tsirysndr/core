package millproto

import (
	"io"
	"sync"

	"github.com/gorilla/websocket"
)

// adapts a gorilla websocket connection to an io.ReadWriteCloser so the
// length-prefixed fleet framing rides over it. each Encode produces exactly one
// binary frame. the reader reassembles the byte stream across frames
type WSStream struct {
	conn *websocket.Conn

	rmu sync.Mutex
	r   io.Reader // current message reader, advanced as frames are consumed

	wmu sync.Mutex
}

func NewWSStream(conn *websocket.Conn) *WSStream {
	return &WSStream{conn: conn}
}

func (s *WSStream) Read(p []byte) (int, error) {
	s.rmu.Lock()
	defer s.rmu.Unlock()
	for {
		if s.r == nil {
			_, r, err := s.conn.NextReader()
			if err != nil {
				return 0, err
			}
			s.r = r
		}
		n, err := s.r.Read(p)
		if err == io.EOF {
			s.r = nil
			if n > 0 {
				return n, nil
			}
			continue
		}
		return n, err
	}
}

func (s *WSStream) Write(p []byte) (int, error) {
	s.wmu.Lock()
	defer s.wmu.Unlock()
	if err := s.conn.WriteMessage(websocket.BinaryMessage, p); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (s *WSStream) Close() error { return s.conn.Close() }
