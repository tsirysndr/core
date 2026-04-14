package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/orm"
)

type spyNotifier struct {
	notify.BaseNotifier
	creates int
	deletes int
	renames int
}

func (s *spyNotifier) NewRepo(_ context.Context, _ *models.Repo)    { s.creates++ }
func (s *spyNotifier) DeleteRepo(_ context.Context, _ *models.Repo) { s.deletes++ }
func (s *spyNotifier) RenameRepo(_ context.Context, _ syntax.DID, _, _ *models.Repo) {
	s.renames++
}

func newTestIngester(t *testing.T) (*Ingester, *spyNotifier) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "test.db")
	d, err := db.Make(context.Background(), path)
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })

	spy := &spyNotifier{}
	ing := &Ingester{
		Db:       d,
		Logger:   slog.New(slog.NewTextHandler(io.Discard, nil)),
		Notifier: spy,
	}
	return ing, spy
}

func seedRepoRow(t *testing.T, ing *Ingester, did, knot, name, rkey, repoDid string) *models.Repo {
	t.Helper()
	tx, err := ing.Db.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	repo := &models.Repo{
		Did:     did,
		Name:    name,
		Knot:    knot,
		Rkey:    rkey,
		RepoDid: repoDid,
	}
	if err := db.AddRepo(tx, repo); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	return repo
}

func ptr[T any](v T) *T { return &v }

func makeEvent(t *testing.T, op string, did, rkey string, record tangled.Repo) *jmodels.Event {
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
			Collection: tangled.RepoNSID,
			RKey:       rkey,
			Record:     raw,
		},
	}
}

func makeDeleteEvent(did, rkey string) *jmodels.Event {
	return &jmodels.Event{
		Did:  did,
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationDelete,
			Collection: tangled.RepoNSID,
			RKey:       rkey,
		},
	}
}

func loadRepo(t *testing.T, ing *Ingester, did, rkey string) *models.Repo {
	t.Helper()
	r, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", did),
		orm.FilterEq("rkey", rkey),
	)
	if err != nil {
		t.Fatalf("GetRepo: %v", err)
	}
	return r
}

func TestIngestRepo_CreateInsertsNewRow(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:        "knot.example",
		Name:        ptr("MyRepo"),
		Description: ptr("a test repo"),
		RepoDid:     ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "myrepo")
	if r.Name != "MyRepo" {
		t.Errorf("name = %q, want %q", r.Name, "MyRepo")
	}
	if r.Description != "a test repo" {
		t.Errorf("description = %q", r.Description)
	}
	if r.RepoDid != "did:plc:repo1" {
		t.Errorf("repoDid = %q", r.RepoDid)
	}
	if spy.creates != 1 {
		t.Errorf("NewRepo called %d times, want 1", spy.creates)
	}
}

func TestIngestRepo_CreateSkipsIfRowExists(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "myrepo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("myrepo"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	if spy.creates != 0 {
		t.Errorf("row already exists, NewRepo should not be called but was called %d times", spy.creates)
	}
}

func TestIngestRepo_CreateCascadesRename(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "oldname", "oldname", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "newname", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("NewName"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	_, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", "did:plc:akshay"),
		orm.FilterEq("rkey", "oldname"),
	)
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("old rkey row should be gone, got err = %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "newname")
	if r.Name != "NewName" {
		t.Errorf("name = %q, want %q", r.Name, "NewName")
	}
	if r.RepoDid != "did:plc:repo1" {
		t.Errorf("repoDid = %q", r.RepoDid)
	}

	hint, err := db.LookupRepoRename(ing.Db, "did:plc:akshay", "oldname")
	if err != nil {
		t.Fatalf("LookupRepoRename: %v", err)
	}
	if hint == nil {
		t.Fatal("expected rename history, got nil")
	}

	if spy.renames != 1 {
		t.Errorf("RenameRepo called %d times, want 1", spy.renames)
	}
	if spy.creates != 0 {
		t.Errorf("rename should not create: NewRepo called %d times, want 0", spy.creates)
	}
}

func TestIngestRepo_CreateNoRepoDidSkipped(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot: "knot.example",
		Name: ptr("myrepo"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times, want 0", spy.creates)
	}
}

func TestIngestRepo_UpdateMetadata(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "foo", tangled.Repo{
		Knot:        "knot.example",
		Name:        ptr("foo"),
		Description: ptr("updated description"),
		Website:     ptr("https://example.com"),
		Topics:      []string{"go", "test"},
		RepoDid:     ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "foo")
	if r.Description != "updated description" {
		t.Errorf("description = %q", r.Description)
	}
	if r.Website != "https://example.com" {
		t.Errorf("website = %q", r.Website)
	}
	if got := r.TopicStr(); got != "go test" {
		t.Errorf("topics = %q", got)
	}
}

func TestIngestRepo_UpdateDisplayName(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "foo", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("Foo"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "foo")
	if r.Name != "Foo" {
		t.Errorf("name = %q, want %q", r.Name, "Foo")
	}
	if r.Rkey != "foo" {
		t.Errorf("rkey should be unchanged but got %q, want %q", r.Rkey, "foo")
	}
}

func TestIngestRepo_UpdateNothingChangedNoOp(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "foo", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("foo"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "foo")
	if r.Name != "foo" {
		t.Errorf("name = %q, want unchanged %q", r.Name, "foo")
	}
}

func TestIngestRepo_UnknownRowSkipped(t *testing.T) {
	ops := []string{jmodels.CommitOperationUpdate, jmodels.CommitOperationDelete}
	for _, op := range ops {
		t.Run(op, func(t *testing.T) {
			ing, _ := newTestIngester(t)

			var e *jmodels.Event
			switch op {
			case jmodels.CommitOperationUpdate:
				e = makeEvent(t, op, "did:plc:nobody", "ghost", tangled.Repo{
					Knot:    "knot.example",
					Name:    ptr("ghost"),
					RepoDid: ptr("did:plc:nope"),
				})
			case jmodels.CommitOperationDelete:
				e = makeDeleteEvent("did:plc:nobody", "ghost")
			}

			if err := ing.ingestRepo(context.Background(), e); err != nil {
				t.Fatalf("ingestRepo: %v", err)
			}
		})
	}
}

func TestIngestRepo_UpdateNoRepoDidSkipped(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "foo", tangled.Repo{
		Knot: "knot.example",
		Name: ptr("bar"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "foo")
	if r.Name != "foo" {
		t.Errorf("name = %q, want unchanged %q", r.Name, "foo")
	}
}

func TestIngestRepo_DeleteRemovesRow(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")

	e := makeDeleteEvent("did:plc:akshay", "foo")
	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	_, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", "did:plc:akshay"),
		orm.FilterEq("rkey", "foo"),
	)
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("expected row to be deleted, got err = %v", err)
	}
}

func TestIngestRepo_MalformedRecord(t *testing.T) {
	ing, _ := newTestIngester(t)

	e := &jmodels.Event{
		Did:  "did:plc:akshay",
		Kind: jmodels.EventKindCommit,
		Commit: &jmodels.Commit{
			Operation:  jmodels.CommitOperationUpdate,
			Collection: tangled.RepoNSID,
			RKey:       "rkey1",
			Record:     json.RawMessage("{not json"),
		},
	}

	if err := ing.ingestRepo(context.Background(), e); err == nil {
		t.Errorf("ingestRepo with malformed record: err = nil, want error")
	}
}

func TestIngestRepo_RenameDeleteSequenceNoTornState(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "oldname", "oldname", "did:plc:repo1")

	if _, err := ing.Db.Exec(
		`insert into stars (did, rkey, subject_type, subject) values (?, ?, ?, ?)`,
		"did:plc:boltless", "star1", "repo", "did:plc:repo1",
	); err != nil {
		t.Fatalf("seed star: %v", err)
	}

	createEvt := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "newname", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("NewName"),
		RepoDid: ptr("did:plc:repo1"),
	})
	if err := ing.ingestRepo(context.Background(), createEvt); err != nil {
		t.Fatalf("ingest create: %v", err)
	}

	deleteEvt := makeDeleteEvent("did:plc:akshay", "oldname")
	if err := ing.ingestRepo(context.Background(), deleteEvt); err != nil {
		t.Fatalf("ingest delete: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "newname")
	if r.Name != "NewName" {
		t.Errorf("name = %q, want %q", r.Name, "NewName")
	}
	if r.RepoDid != "did:plc:repo1" {
		t.Errorf("repoDid = %q, want %q", r.RepoDid, "did:plc:repo1")
	}

	_, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", "did:plc:akshay"),
		orm.FilterEq("rkey", "oldname"),
	)
	if !errors.Is(err, sql.ErrNoRows) {
		t.Errorf("old rkey should be gone, got err = %v", err)
	}

	var starSubject string
	if err := ing.Db.QueryRow(`select subject from stars where did = ?`, "did:plc:boltless").Scan(&starSubject); err != nil {
		t.Fatalf("query star: %v", err)
	}
	if starSubject != "did:plc:repo1" {
		t.Errorf("star subject = %q, want %q", starSubject, "did:plc:repo1")
	}

	if spy.renames != 1 {
		t.Errorf("RenameRepo called %d times, want 1", spy.renames)
	}
	if spy.creates != 0 {
		t.Errorf("rename should not create: NewRepo called %d times, want 0", spy.creates)
	}
	if spy.deletes != 0 {
		t.Errorf("old rkey already gone, DeleteRepo should not be called but was called %d times", spy.deletes)
	}
}

func TestIngestRepo_CreateFallsBackToRkeyForName(t *testing.T) {
	ing, _ := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ing.ingestRepo(context.Background(), e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "myrepo")
	if r.Name != "myrepo" {
		t.Errorf("name should fall back to rkey: got %q, want %q", r.Name, "myrepo")
	}
}
