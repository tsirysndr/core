package millproto

import (
	"bytes"
	"encoding/binary"
	"testing"

	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

func TestEncodeDecodeRoundTrip(t *testing.T) {
	var buf bytes.Buffer
	enc := NewEncoder(&buf)

	want := &Message{
		ReserveSeat: &millv1.ReserveSeat{
			LeaseId:         "lease-1",
			TargetEngine:    "microvm",
			RawWorkflowJson: `{"name":"build"}`,
			Knot:            "knot.example",
			Rkey:            "abc123",
			TtlSeconds:      30,
		},
	}
	if err := enc.Encode(want); err != nil {
		t.Fatalf("Encode() error = %v", err)
	}

	got, err := NewDecoder(&buf).Decode()
	if err != nil {
		t.Fatalf("Decode() error = %v", err)
	}
	rs := got.GetReserveSeat()
	if rs == nil {
		t.Fatal("decoded message missing reserve_seat")
	}
	if rs.LeaseId != "lease-1" || rs.TargetEngine != "microvm" || rs.TtlSeconds != 30 {
		t.Fatalf("round-trip mismatch: %+v", rs)
	}
}

func TestDecoderRejectsOversizedMessage(t *testing.T) {
	var tooLarge bytes.Buffer
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], MaxMessageBytes+1)
	tooLarge.Write(header[:])

	if _, err := NewDecoder(&tooLarge).Decode(); err == nil {
		t.Fatal("expected oversized message error")
	}
}

func TestValidationRules(t *testing.T) {
	tests := []struct {
		name    string
		msg     *Message
		wantErr bool
	}{
		{
			name: "valid ack message",
			msg: &Message{
				Ack: &millv1.Ack{
					Epoch:     "inc-1",
					UpToSeqno: 5,
				},
			},
			wantErr: false,
		},
		{
			name: "valid hello message",
			msg: &Message{
				Hello: &millv1.Hello{
					ProtocolVersion: 1,
					Arch:            "amd64",
					Labels:          []string{"linux"},
					Epoch:           "inc-1",
				},
			},
			wantErr: false,
		},
		{
			name:    "invalid message with zero payloads",
			msg:     &Message{},
			wantErr: true,
		},
		{
			name: "invalid message with multiple payloads",
			msg: &Message{
				Ack:       &millv1.Ack{Epoch: "inc-1", UpToSeqno: 5},
				Committed: &millv1.Committed{LeaseId: "x"},
			},
			wantErr: true,
		},
		{
			name: "invalid ack message missing epoch",
			msg: &Message{
				Ack: &millv1.Ack{
					UpToSeqno: 5,
				},
			},
			wantErr: true,
		},
		{
			name: "invalid node snapshot with zero seqno",
			msg: &Message{
				NodeSnapshot: &millv1.NodeSnapshot{
					Seqno: 0,
				},
			},
			wantErr: true,
		},
		{
			name: "valid node snapshot with positive seqno",
			msg: &Message{
				NodeSnapshot: &millv1.NodeSnapshot{
					Seqno: 1,
				},
			},
			wantErr: false,
		},
		{
			name: "invalid stream batch with zero seqno entry",
			msg: &Message{
				EventBatch: &millv1.EventBatch{
					Epoch: "inc-1",
					Events: []*millv1.Event{
						{
							Seqno:   0,
							LeaseId: "lease-1",
							Payload: &millv1.Event_StatusEvent{
								StatusEvent: &millv1.StatusEvent{
									Status: millv1.NonterminalStatus_RUNNING,
								},
							},
						},
					},
				},
			},
			wantErr: true,
		},
		{
			name: "invalid stream batch with empty entries",
			msg: &Message{
				EventBatch: &millv1.EventBatch{
					Epoch:  "inc-1",
					Events: []*millv1.Event{},
				},
			},
			wantErr: true,
		},
		{
			name: "invalid reserve result with unknown enum",
			msg: &Message{
				ReserveResult: &millv1.ReserveResult{
					LeaseId:     "lease-1",
					RejectClass: millv1.RejectClass(99),
				},
			},
			wantErr: true,
		},
		{
			name: "invalid stream entry with malformed oneof (empty payload)",
			msg: &Message{
				EventBatch: &millv1.EventBatch{
					Epoch: "inc-1",
					Events: []*millv1.Event{
						{
							Seqno:   1,
							LeaseId: "lease-1",
							Payload: nil,
						},
					},
				},
			},
			wantErr: true,
		},
		{
			name: "valid stream batch status event",
			msg: &Message{
				EventBatch: &millv1.EventBatch{
					Epoch: "inc-1",
					Events: []*millv1.Event{
						{
							Seqno:   1,
							LeaseId: "lease-1",
							Payload: &millv1.Event_StatusEvent{
								StatusEvent: &millv1.StatusEvent{
									Status: millv1.NonterminalStatus_RUNNING,
								},
							},
						},
					},
				},
			},
			wantErr: false,
		},
		{
			name: "valid stream batch attempt result",
			msg: &Message{
				EventBatch: &millv1.EventBatch{
					Epoch: "inc-1",
					Events: []*millv1.Event{
						{
							Seqno:   1,
							LeaseId: "lease-1",
							Payload: &millv1.Event_AttemptResult{
								AttemptResult: &millv1.AttemptResult{
									Status: millv1.TerminalStatus_SUCCESS,
								},
							},
						},
					},
				},
			},
			wantErr: false,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := validator.Validate(tc.msg)
			if (err != nil) != tc.wantErr {
				t.Fatalf("Validate() error = %v, wantErr = %v", err, tc.wantErr)
			}
		})
	}
}
