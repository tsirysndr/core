package executor

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"testing"

	"tangled.org/core/api/tangled"
	millv1 "tangled.org/core/spindle/mill/proto/gen"
	"tangled.org/core/spindle/models"
)

type placementEngine struct {
	*fakeEngine
	validationErr error
	validated     bool
}

func (e *placementEngine) ValidateWorkflowPlacement(*models.Workflow) error {
	e.validated = true
	return e.validationErr
}

func TestHandleReserveRejectsMissingTriggerMetadata(t *testing.T) {
	enc := newCaptureEncoder()
	e := &Executor{
		l:       slog.New(slog.NewTextHandler(io.Discard, nil)),
		enc:     enc,
		seats:   1,
		engines: map[string]models.Engine{"microvm": &fakeEngine{}},
		active:  make(map[string]*reservation),
	}
	twf, err := json.Marshal(tangled.Pipeline_Workflow{Name: "build"})
	if err != nil {
		t.Fatal(err)
	}
	tpl, err := json.Marshal(tangled.Pipeline{})
	if err != nil {
		t.Fatal(err)
	}

	// engines deref TriggerMetadata unconditionally. this must be a reject,
	// not a panic that takes the whole executor down
	e.handleReserve(context.Background(), &millv1.ReserveSeat{
		LeaseId:         "lease-1",
		TargetEngine:    "microvm",
		RawWorkflowJson: string(twf),
		RawPipelineJson: string(tpl),
		Knot:            "k",
		Rkey:            "r",
	})

	result := (<-enc.messages).GetReserveResult()
	if result == nil {
		t.Fatal("handleReserve() did not send ReserveResult")
	}
	if result.GetAccepted() {
		t.Fatal("handleReserve() accepted a pipeline without trigger metadata")
	}
	if result.GetRejectClass() != millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE {
		t.Fatalf("reject class = %v, want incompatible", result.GetRejectClass())
	}
	if len(e.active) != 0 {
		t.Fatalf("active reservations = %d, want 0", len(e.active))
	}
}

func TestHandleReserveValidatesPlacementBeforeAcquiringSlot(t *testing.T) {
	enc := newCaptureEncoder()
	validationErr := errors.New("image architecture is not native")
	eng := &placementEngine{fakeEngine: &fakeEngine{}, validationErr: validationErr}
	e := &Executor{
		l:       slog.New(slog.NewTextHandler(io.Discard, nil)),
		enc:     enc,
		seats:   1,
		engines: map[string]models.Engine{"microvm": eng},
		active:  make(map[string]*reservation),
	}
	twf, err := json.Marshal(tangled.Pipeline_Workflow{Name: "build"})
	if err != nil {
		t.Fatal(err)
	}
	tpl, err := json.Marshal(tangled.Pipeline{TriggerMetadata: &tangled.Pipeline_TriggerMetadata{}})
	if err != nil {
		t.Fatal(err)
	}

	e.handleReserve(context.Background(), &millv1.ReserveSeat{
		LeaseId:         "lease-1",
		TargetEngine:    "microvm",
		RawWorkflowJson: string(twf),
		RawPipelineJson: string(tpl),
		Knot:            "k",
		Rkey:            "r",
	})

	result := (<-enc.messages).GetReserveResult()
	if result == nil {
		t.Fatal("handleReserve() did not send ReserveResult")
	}
	if result.GetAccepted() {
		t.Fatal("handleReserve() accepted placement validation failure")
	}
	if result.GetRejectClass() != millv1.RejectClass_REJECT_CLASS_INCOMPATIBLE {
		t.Fatalf("reject class = %v, want incompatible", result.GetRejectClass())
	}
	if !eng.validated {
		t.Fatal("placement validator was not called")
	}
	if eng.acquireCalled {
		t.Fatal("slot acquisition ran after placement validation failed")
	}
	if len(e.active) != 0 {
		t.Fatalf("active reservations = %d, want 0", len(e.active))
	}
}
