// package millproto carries the mill<->executor session protocol. a new message
// vocabulary over the same length-prefixed protobuf framing the spindle already
// uses to talk to the microVM guest (see spindle/agentproto). only the framing
// pattern is shared. the messages are entirely separate
package millproto

import (
	"encoding/binary"
	"fmt"
	"io"
	"sync"

	"buf.build/go/protovalidate"
	"google.golang.org/protobuf/proto"

	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

const (
	ProtocolVersion = 1
	// generous vs agentproto's 1 MiB. a ReserveSeat carries the raw pipeline and
	// workflow JSON, and streamed log lines can be chunky
	MaxMessageBytes = 8 * 1024 * 1024
)

type Message = millv1.Message

var validator protovalidate.Validator

func init() {
	var err error
	validator, err = protovalidate.New()
	if err != nil {
		panic(fmt.Errorf("failed to initialize protovalidate validator: %w", err))
	}
}

type Encoder struct {
	mu sync.Mutex
	w  io.Writer
}

func NewEncoder(w io.Writer) *Encoder {
	return &Encoder{w: w}
}

func (e *Encoder) Encode(msg *Message) error {
	if err := validator.Validate(msg); err != nil {
		return fmt.Errorf("validate fleet message: %w", err)
	}

	data, err := proto.Marshal(msg)
	if err != nil {
		return fmt.Errorf("marshal fleet message: %w", err)
	}
	if len(data) > MaxMessageBytes {
		return fmt.Errorf("fleet message exceeded %d bytes", MaxMessageBytes)
	}

	// single write of header and payload maps to exactly one websocket binary
	// frame when the writer is a ws stream
	frame := make([]byte, 4+len(data))
	binary.BigEndian.PutUint32(frame[:4], uint32(len(data)))
	copy(frame[4:], data)

	e.mu.Lock()
	defer e.mu.Unlock()
	_, err = e.w.Write(frame)
	return err
}

type Decoder struct {
	r io.Reader
}

func NewDecoder(r io.Reader) *Decoder {
	return &Decoder{r: r}
}

func (d *Decoder) Decode() (*Message, error) {
	msg := &Message{}
	var header [4]byte
	if _, err := io.ReadFull(d.r, header[:]); err != nil {
		return msg, err
	}

	size := binary.BigEndian.Uint32(header[:])
	if size > MaxMessageBytes {
		return msg, fmt.Errorf("fleet message exceeded %d bytes", MaxMessageBytes)
	}

	data := make([]byte, size)
	if _, err := io.ReadFull(d.r, data); err != nil {
		return msg, err
	}
	if err := proto.Unmarshal(data, msg); err != nil {
		return msg, fmt.Errorf("parse fleet message: %w", err)
	}
	if err := validator.Validate(msg); err != nil {
		return msg, fmt.Errorf("validate fleet message: %w", err)
	}
	return msg, nil
}
