package agentproto

import (
	"encoding/binary"
	"fmt"
	"io"
	"sync"

	"google.golang.org/protobuf/proto"

	"buf.build/go/protovalidate"
	agentv1 "tangled.org/core/spindle/agentproto/gen"
)

const (
	ProtocolVersion = 1
	DefaultPort     = 10240
	MaxMessageBytes = 1024 * 1024
)

type Message = agentv1.Message

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
		return fmt.Errorf("validate agent message: %w", err)
	}

	data, err := proto.Marshal(msg)
	if err != nil {
		return fmt.Errorf("marshal agent message: %w", err)
	}
	if len(data) > MaxMessageBytes {
		return fmt.Errorf("agent message exceeded %d bytes", MaxMessageBytes)
	}

	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(data)))

	e.mu.Lock()
	defer e.mu.Unlock()
	if _, err := e.w.Write(header[:]); err != nil {
		return err
	}
	_, err = e.w.Write(data)
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
		return msg, fmt.Errorf("agent message exceeded %d bytes", MaxMessageBytes)
	}

	data := make([]byte, size)
	if _, err := io.ReadFull(d.r, data); err != nil {
		return msg, err
	}
	if err := proto.Unmarshal(data, msg); err != nil {
		return msg, fmt.Errorf("parse agent message: %w", err)
	}
	if err := validator.Validate(msg); err != nil {
		return msg, fmt.Errorf("validate agent message: %w", err)
	}
	return msg, nil
}
