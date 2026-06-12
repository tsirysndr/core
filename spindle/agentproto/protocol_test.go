package agentproto

import (
	"bytes"
	"encoding/binary"
	"testing"

	agentv1 "tangled.org/core/spindle/agentproto/gen"
)

func TestDecoderRejectsOversizedMessage(t *testing.T) {
	var tooLarge bytes.Buffer
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], MaxMessageBytes+1)
	tooLarge.Write(header[:])

	_, err := NewDecoder(&tooLarge).Decode()
	if err == nil {
		t.Fatal("expected oversized message error")
	}
}

func TestValidation(t *testing.T) {
	// 1. Valid message (exactly one of the payload fields is set)
	validMsg := &Message{
		Id: "test-1",
		Hello: &agentv1.Hello{
			ProtocolVersion: 1,
			AgentVersion:    "1.0",
		},
	}
	if err := validator.Validate(validMsg); err != nil {
		t.Fatalf("expected valid message to pass validation, got: %v", err)
	}

	// 2. Invalid message: zero payloads set
	invalidZeroMsg := &Message{
		Id: "test-2",
	}
	if err := validator.Validate(invalidZeroMsg); err == nil {
		t.Fatal("expected message with zero payloads to fail validation")
	}

	// 3. Invalid message: multiple payloads set
	invalidMultiMsg := &Message{
		Id: "test-3",
		Hello: &agentv1.Hello{
			ProtocolVersion: 1,
		},
		Init: &agentv1.Init{
			JobId: "job-1",
		},
	}
	if err := validator.Validate(invalidMultiMsg); err == nil {
		t.Fatal("expected message with multiple payloads to fail validation")
	}
}
