package appview

import (
	"encoding/json"
	"io"
	"log/slog"
	"path/filepath"
	"strings"
	"testing"
	"time"

	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/validator"
	"tangled.org/core/orm"
)

func newStringIngester(t *testing.T) *Ingester {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := db.Make(t.Context(), path)
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return &Ingester{
		Db:        d,
		Logger:    slog.New(slog.NewTextHandler(io.Discard, nil)),
		Validator: &validator.Validator{},
	}
}

func makeStringEvent(t *testing.T, op, did, rkey string, record tangled.String) *jmodels.Event {
	t.Helper()
	raw, err := json.Marshal(record)
	if err != nil {
		t.Fatalf("marshal record: %v", err)
	}
	return &jmodels.Event{
		Did:  did,
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  op,
			Collection: tangled.StringNSID,
			RKey:       rkey,
			Record:     raw,
		},
	}
}

func loadString(t *testing.T, ing *Ingester, did, rkey string) (models.String, bool) {
	t.Helper()
	rows, err := db.GetStrings(ing.Db, 0,
		orm.FilterEq("did", did),
		orm.FilterEq("rkey", rkey),
	)
	if err != nil {
		t.Fatalf("GetStrings: %v", err)
	}
	if len(rows) == 0 {
		return models.String{}, false
	}
	if len(rows) > 1 {
		t.Fatalf("expected at most one row for (%s,%s), got %d", did, rkey, len(rows))
	}
	return rows[0], true
}

func TestIngestString_CreateRoundTrip(t *testing.T) {
	ing := newStringIngester(t)
	created := time.Date(2025, 9, 14, 10, 30, 0, 0, time.UTC)

	e := makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "rk1", tangled.String{
		Filename:    "hello.txt",
		Description: "a greeting",
		Contents:    "hello world\n",
		CreatedAt:   created.Format(time.RFC3339),
	})

	if err := ing.ingestString(e); err != nil {
		t.Fatalf("ingestString: %v", err)
	}

	s, ok := loadString(t, ing, "did:plc:boltless", "rk1")
	if !ok {
		t.Fatal("row not inserted")
	}
	if s.Filename != "hello.txt" {
		t.Errorf("filename = %q, want hello.txt", s.Filename)
	}
	if s.Description != "a greeting" {
		t.Errorf("description = %q", s.Description)
	}
	if s.Contents != "hello world\n" {
		t.Errorf("contents = %q", s.Contents)
	}
	if !s.Created.Equal(created) {
		t.Errorf("created = %v, want %v (record CreatedAt must round-trip)", s.Created, created)
	}
	if s.Edited != nil {
		t.Errorf("edited = %v, want nil on create", s.Edited)
	}
}

func TestIngestString_UpdateBumpsEditedOnContentChange(t *testing.T) {
	ing := newStringIngester(t)
	created := time.Date(2025, 9, 14, 10, 30, 0, 0, time.UTC)
	base := tangled.String{
		Filename:    "hello.txt",
		Description: "a greeting",
		Contents:    "hello world\n",
		CreatedAt:   created.Format(time.RFC3339),
	}

	if err := ing.ingestString(makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "rk1", base)); err != nil {
		t.Fatalf("ingestString create: %v", err)
	}

	updated := base
	updated.Contents = "hello, world!\n"
	if err := ing.ingestString(makeStringEvent(t, jmodels.CommitOperationUpdate, "did:plc:boltless", "rk1", updated)); err != nil {
		t.Fatalf("ingestString update: %v", err)
	}

	s, ok := loadString(t, ing, "did:plc:boltless", "rk1")
	if !ok {
		t.Fatal("row missing after update")
	}
	if s.Contents != "hello, world!\n" {
		t.Errorf("contents = %q, want updated value", s.Contents)
	}
	if !s.Created.Equal(created) {
		t.Errorf("update overwrote created: got %v, want %v", s.Created, created)
	}
	if s.Edited == nil {
		t.Fatal("edited not set after content change")
	}
}

func TestIngestString_UpdateNoChangeKeepsEditedNil(t *testing.T) {
	ing := newStringIngester(t)
	rec := tangled.String{
		Filename:    "hello.txt",
		Description: "a greeting",
		Contents:    "hello world\n",
		CreatedAt:   time.Date(2025, 9, 14, 10, 30, 0, 0, time.UTC).Format(time.RFC3339),
	}

	if err := ing.ingestString(makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "rk2", rec)); err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := ing.ingestString(makeStringEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "rk2", rec)); err != nil {
		t.Fatalf("update: %v", err)
	}

	s, _ := loadString(t, ing, "did:plc:akshay", "rk2")
	if s.Edited != nil {
		t.Errorf("edited = %v, want nil when no field changed", s.Edited)
	}
}

func TestIngestString_DeleteRemovesRow(t *testing.T) {
	ing := newStringIngester(t)
	rec := tangled.String{
		Filename:  "hello.txt",
		Contents:  "x",
		CreatedAt: time.Now().UTC().Format(time.RFC3339),
	}
	if err := ing.ingestString(makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "rk1", rec)); err != nil {
		t.Fatalf("create: %v", err)
	}

	del := &jmodels.Event{
		Did:  "did:plc:boltless",
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationDelete,
			Collection: tangled.StringNSID,
			RKey:       "rk1",
		},
	}
	if err := ing.ingestString(del); err != nil {
		t.Fatalf("delete: %v", err)
	}

	if _, ok := loadString(t, ing, "did:plc:boltless", "rk1"); ok {
		t.Fatal("row still present after delete")
	}
}

func TestIngestString_ValidatorRejects(t *testing.T) {
	ing := newStringIngester(t)
	now := time.Now().UTC().Format(time.RFC3339)

	cases := []struct {
		name string
		rec  tangled.String
	}{
		{"empty contents", tangled.String{Filename: "x", Contents: "", CreatedAt: now}},
		{"long filename", tangled.String{Filename: strings.Repeat("a", 141), Contents: "x", CreatedAt: now}},
		{"long description", tangled.String{Filename: "x", Description: strings.Repeat("d", 281), Contents: "x", CreatedAt: now}},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			e := makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "bad", tc.rec)
			if err := ing.ingestString(e); err == nil {
				t.Fatal("expected validator error, got nil")
			}
			if _, ok := loadString(t, ing, "did:plc:akshay", "bad"); ok {
				t.Fatal("row inserted despite validator failure")
			}
		})
	}
}

func TestIngestString_ColdReplayPreservesCreated(t *testing.T) {
	created := time.Date(2024, 1, 2, 3, 4, 5, 0, time.UTC)
	rec := tangled.String{
		Filename:    "snippet.go",
		Description: "first cut",
		Contents:    "package main\n",
		CreatedAt:   created.Format(time.RFC3339),
	}
	event := makeStringEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "rkcold", rec)

	first := newStringIngester(t)
	if err := first.ingestString(event); err != nil {
		t.Fatalf("first ingest: %v", err)
	}
	live, _ := loadString(t, first, "did:plc:boltless", "rkcold")

	second := newStringIngester(t)
	if err := second.ingestString(event); err != nil {
		t.Fatalf("replay ingest: %v", err)
	}
	replayed, _ := loadString(t, second, "did:plc:boltless", "rkcold")

	if !live.Created.Equal(replayed.Created) {
		t.Fatalf("cold replay drifted: live=%v replayed=%v", live.Created, replayed.Created)
	}
	if !replayed.Created.Equal(created) {
		t.Fatalf("replay lost record CreatedAt: got %v, want %v", replayed.Created, created)
	}
}
