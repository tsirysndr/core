package spindle

import (
	"context"
	"encoding/json"
	"log/slog"
	"strings"
	"tangled.org/core/jetstream"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/jetstream/pkg/models"

	"tangled.org/core/api/tangled"
	"tangled.org/core/rbac"
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

func TestIngestMember_RBAC(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}

	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		jc:      jc,
		rootCtx: context.Background(),
	}

	actorDid := "did:plc:adminactor"
	subjectDid := "did:plc:newmember"
	rbacDomain := rbac.ThisServer

	memberRecord := tangled.SpindleMember{
		Instance: "spindle.test",
		Subject:  subjectDid,
	}
	memberRecordJson, _ := json.Marshal(memberRecord)

	evt := &models.Event{
		Did:  actorDid,
		Kind: models.EventKindCommit,
		Commit: &models.Commit{
			Operation:  models.CommitOperationCreate,
			Collection: tangled.SpindleMemberNSID,
			RKey:       "member-rkey-1",
			Record:     memberRecordJson,
		},
	}

	err := s.ingestMember(context.Background(), evt)
	if err == nil {
		t.Fatal("expected permission denied error, got nil")
	}
	if !strings.Contains(err.Error(), "permission denied") {
		t.Fatalf("expected permission denied, got error: %v", err)
	}

	var dbCount int
	err = d.QueryRow(`select count(*) from spindle_members where subject = ?`, subjectDid).Scan(&dbCount)
	if err != nil {
		t.Fatalf("DB query error: %v", err)
	}
	if dbCount > 0 {
		t.Fatal("spindle member was registered in DB on failed auth")
	}

	err = e.AddSpindle(rbacDomain)
	if err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}
	err = e.AddSpindleOwner(rbacDomain, actorDid)
	if err != nil {
		t.Fatalf("AddSpindleOwner: %v", err)
	}

	err = s.ingestMember(context.Background(), evt)
	if err != nil {
		t.Fatalf("ingestMember failed for authorized actor: %v", err)
	}

	err = d.QueryRow(`select count(*) from spindle_members where subject = ?`, subjectDid).Scan(&dbCount)
	if err != nil || dbCount != 1 {
		t.Fatalf("expected exactly 1 member in DB, got: %d (err: %v)", dbCount, err)
	}

	isMember, err := e.IsSpindleMember(subjectDid, rbacDomain)
	if err != nil || !isMember {
		t.Fatalf("expected subject to be spindle member in Casbin, got: %t (err: %v)", isMember, err)
	}

	deleteEvt := &models.Event{
		Did:  actorDid,
		Kind: models.EventKindCommit,
		Commit: &models.Commit{
			Operation:  models.CommitOperationDelete,
			Collection: tangled.SpindleMemberNSID,
			RKey:       "member-rkey-1",
		},
	}

	err = s.ingestMember(context.Background(), deleteEvt)
	if err != nil {
		t.Fatalf("ingestMember delete failed: %v", err)
	}

	err = d.QueryRow(`select count(*) from spindle_members where subject = ?`, subjectDid).Scan(&dbCount)
	if err != nil || dbCount != 0 {
		t.Fatalf("expected 0 members in DB after delete, got: %d (err: %v)", dbCount, err)
	}

	isMember, err = e.IsSpindleMember(subjectDid, rbacDomain)
	if err != nil || isMember {
		t.Fatalf("expected subject to NOT be spindle member in Casbin, got: %t (err: %v)", isMember, err)
	}
}

func TestIngestMember_ForgeDeleteRejection(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}

	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		jc:      jc,
		rootCtx: context.Background(),
	}

	adminDid := "did:plc:adminactor"
	bobDid := "did:plc:bobactor"
	subjectDid := "did:plc:newmember"
	rbacDomain := rbac.ThisServer

	err := e.AddSpindle(rbacDomain)
	if err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}
	err = e.AddSpindleOwner(rbacDomain, adminDid)
	if err != nil {
		t.Fatalf("AddSpindleOwner: %v", err)
	}

	memberRecord := tangled.SpindleMember{
		Instance: "spindle.test",
		Subject:  subjectDid,
	}
	memberRecordJson, _ := json.Marshal(memberRecord)

	evt := &models.Event{
		Did:  adminDid,
		Kind: models.EventKindCommit,
		Commit: &models.Commit{
			Operation:  models.CommitOperationCreate,
			Collection: tangled.SpindleMemberNSID,
			RKey:       "member-rkey-1",
			Record:     memberRecordJson,
		},
	}

	err = s.ingestMember(context.Background(), evt)
	if err != nil {
		t.Fatalf("ingestMember failed for admin: %v", err)
	}

	var dbCount int
	err = d.QueryRow(`select count(*) from spindle_members where subject = ?`, subjectDid).Scan(&dbCount)
	if err != nil || dbCount != 1 {
		t.Fatalf("expected member in DB, got: %d (err: %v)", dbCount, err)
	}

	isMember, err := e.IsSpindleMember(subjectDid, rbacDomain)
	if err != nil || !isMember {
		t.Fatalf("expected subject to be spindle member, got %t (err: %v)", isMember, err)
	}

	// bob tries to delete alice's spindle member record, must reject forged delete
	deleteEvt := &models.Event{
		Did:  bobDid, // Bob is the actor
		Kind: models.EventKindCommit,
		Commit: &models.Commit{
			Operation:  models.CommitOperationDelete,
			Collection: tangled.SpindleMemberNSID,
			RKey:       "member-rkey-1",
		},
	}

	err = s.ingestMember(context.Background(), deleteEvt)
	if err != nil {
		t.Fatalf("ingestMember delete returned error: %v", err)
	}

	err = d.QueryRow(`select count(*) from spindle_members where subject = ?`, subjectDid).Scan(&dbCount)
	if err != nil || dbCount != 1 {
		t.Fatalf("member was deleted from DB, expected remaining, count: %d (err: %v)", dbCount, err)
	}

	isMember, err = e.IsSpindleMember(subjectDid, rbacDomain)
	if err != nil || !isMember {
		t.Fatal("member policy was removed from Casbin by forged delete")
	}
}
