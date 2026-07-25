package spindle

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/jetstream/pkg/models"

	"tangled.org/core/api/tangled"
	"tangled.org/core/spindle/config"
	"tangled.org/core/tapc"
)

func TestTapProcessEventIgnoresPullRecords(t *testing.T) {
	client := &Tap{}
	err := client.processEvent(context.Background(), tapc.Event{
		Type: tapc.EvtRecord,
		Record: &tapc.RecordEventData{
			Live:       true,
			Did:        syntax.DID("did:plc:jge3zxi7lgrfnvhzcgrimeo7"),
			Collection: syntax.NSID(tangled.RepoPullNSID),
			Rkey:       syntax.RecordKey("3mrhpypucbsg4"),
			Action:     tapc.RecordCreateAction,
			Record:     json.RawMessage(`{`),
		},
	})
	if err != nil {
		t.Fatalf("Tap.processEvent() returned an error for a pull record: %v", err)
	}
}

func TestJetstreamToTapEventMarksPullRecordsLive(t *testing.T) {
	tests := []struct {
		name      string
		operation string
		action    tapc.RecordAction
	}{
		{name: "create", operation: models.CommitOperationCreate, action: tapc.RecordCreateAction},
		{name: "update", operation: models.CommitOperationUpdate, action: tapc.RecordUpdateAction},
		{name: "delete", operation: models.CommitOperationDelete, action: tapc.RecordDeleteAction},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			event, ok := jetstreamToTapEvent(&models.Event{
				Did:  "did:plc:jge3zxi7lgrfnvhzcgrimeo7",
				Kind: models.EventKindCommit,
				Commit: &models.Commit{
					Operation:  tt.operation,
					Collection: tangled.RepoPullNSID,
					RKey:       "3mrhpypucbsg4",
					Record:     json.RawMessage(`{"title":"test"}`),
				},
			})
			if !ok {
				t.Fatal("jetstreamToTapEvent() rejected a valid pull event")
			}
			if event.Record == nil {
				t.Fatal("jetstreamToTapEvent() returned no record")
			}
			if !event.Record.Live {
				t.Error("converted pull event is not live")
			}
			if event.Record.Collection.String() != tangled.RepoPullNSID {
				t.Errorf("collection = %q, want %q", event.Record.Collection, tangled.RepoPullNSID)
			}
			if event.Record.Action != tt.action {
				t.Errorf("action = %q, want %q", event.Record.Action, tt.action)
			}
		})
	}
}

func TestEmbeddedTapDoesNotSubscribeToPullRecords(t *testing.T) {
	tcfg := newEmbeddedTapConfig(&config.Config{})

	if tcfg.SignalCollection == tangled.RepoPullNSID {
		t.Errorf("SignalCollection = %q, must not ingest pull records", tcfg.SignalCollection)
	}
	for _, collection := range tcfg.CollectionFilters {
		if collection == tangled.RepoPullNSID {
			t.Errorf("CollectionFilters includes %q", tangled.RepoPullNSID)
		}
	}
}
