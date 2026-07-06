package appview

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"
	"net/url"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	jmodels "github.com/bluesky-social/jetstream/pkg/models"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/repoverify"
)

func mustKnotURL(t *testing.T, raw string) *url.URL {
	t.Helper()
	u, err := repoverify.ParseKnotEndpoint(raw, true)
	if err != nil {
		t.Fatalf("ParseKnotEndpoint(%q): %v", raw, err)
	}
	return u
}

func acceptOwner(t *testing.T, e *jmodels.Event) repoverify.Verifier {
	t.Helper()
	knot := mustKnotURL(t, "https://knot.example")
	return func(_ context.Context, repoDid repoverify.RepoDid) (repoverify.Result, error) {
		return repoverify.Result{
			RepoDid:  repoDid,
			OwnerDid: repoverify.OwnerDid(e.Did),
			KnotURL:  knot,
		}, nil
	}
}

func stubVerifier(result repoverify.Result, err error) repoverify.Verifier {
	return func(_ context.Context, _ repoverify.RepoDid) (repoverify.Result, error) {
		return result, err
	}
}

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
	enforcer, err := rbac.NewEnforcer(path)
	if err != nil {
		t.Fatalf("rbac.NewEnforcer: %v", err)
	}

	spy := &spyNotifier{}
	ing := &Ingester{
		Db:       d,
		Enforcer: enforcer,
		Logger:   slog.New(slog.DiscardHandler),
		Notifier: spy,
	}
	return ing, spy
}

func withVerifier(ing *Ingester, v repoverify.Verifier) *Ingester {
	ing.Verifier = v
	return ing
}

func ingestAcceptingOwner(t *testing.T, ing *Ingester, e *jmodels.Event) error {
	t.Helper()
	ing.Verifier = acceptOwner(t, e)
	return ing.ingestRepo(context.Background(), e, ing.Logger)
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

func assertRepoOwnerPermissions(t *testing.T, ing *Ingester, owner, knot, repo string) {
	t.Helper()
	for _, perm := range []string{"repo:settings", "repo:push", "repo:owner"} {
		ok, err := ing.Enforcer.E.Enforce(owner, knot, repo, perm)
		if err != nil {
			t.Fatalf("Enforce(%q): %v", perm, err)
		}
		if !ok {
			t.Fatalf("owner missing %s permission for %s", perm, repo)
		}
	}
}

func assertNoRepoPolicies(t *testing.T, ing *Ingester, knot, repo string) {
	t.Helper()
	for _, perm := range []string{"repo:settings", "repo:push", "repo:owner", "repo:delete", "repo:invite", "repo:collaborator"} {
		policies, err := ing.Enforcer.E.GetFilteredPolicy(1, knot, repo, perm)
		if err != nil {
			t.Fatalf("GetFilteredPolicy(%q): %v", perm, err)
		}
		if len(policies) != 0 {
			t.Fatalf("expected no %s policies for %s, got %v", perm, repo, policies)
		}
	}
}

func TestIngestRepo_CreateInsertsNewRow(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:        "knot.example",
		Name:        ptr("MyRepo"),
		Description: ptr("a test repo"),
		RepoDid:     ptr("did:plc:repo1"),
	})

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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
	assertRepoOwnerPermissions(t, ing, "did:plc:akshay", "knot.example", "did:plc:repo1")
}

func TestIngestRepo_CreateSkipsIfRowExists(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "myrepo", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("myrepo"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	if spy.creates != 0 {
		t.Errorf("row already exists, NewRepo should not be called but was called %d times", spy.creates)
	}
	assertRepoOwnerPermissions(t, ing, "did:plc:akshay", "knot.example", "did:plc:repo1")
}

func TestIngestRepo_CreateCascadesRename(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "oldname", "oldname", "did:plc:repo1")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "newname", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("NewName"),
		RepoDid: ptr("did:plc:repo1"),
	})

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

			if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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
	if err := ingestAcceptingOwner(t, ing, e); err != nil {
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

func TestIngestRepo_DeleteWipesRbac(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "foo", "foo", "did:plc:repo1")
	if err := ing.ensureRepoOwnerPermissions("did:plc:akshay", "knot.example", "did:plc:repo1"); err != nil {
		t.Fatalf("ensureRepoOwnerPermissions: %v", err)
	}
	if err := ing.Enforcer.AddCollaborator("did:plc:boltless", "knot.example", "did:plc:repo1"); err != nil {
		t.Fatalf("AddCollaborator: %v", err)
	}
	if err := ing.Enforcer.E.SavePolicy(); err != nil {
		t.Fatalf("SavePolicy: %v", err)
	}
	assertRepoOwnerPermissions(t, ing, "did:plc:akshay", "knot.example", "did:plc:repo1")

	e := makeDeleteEvent("did:plc:akshay", "foo")
	if err := ingestAcceptingOwner(t, ing, e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	assertNoRepoPolicies(t, ing, "knot.example", "did:plc:repo1")
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

	if err := ingestAcceptingOwner(t, ing, e); err == nil {
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
	if err := ingestAcceptingOwner(t, ing, createEvt); err != nil {
		t.Fatalf("ingest create: %v", err)
	}

	deleteEvt := makeDeleteEvent("did:plc:akshay", "oldname")
	if err := ingestAcceptingOwner(t, ing, deleteEvt); err != nil {
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

	if err := ingestAcceptingOwner(t, ing, e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "myrepo")
	if r.Name != "myrepo" {
		t.Errorf("name should fall back to rkey: got %q, want %q", r.Name, "myrepo")
	}
}

func TestIngestRepo_CreateSquatRejected(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "squatrepo", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	if _, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", "did:plc:boltless"),
		orm.FilterEq("rkey", "squatrepo"),
	); !errors.Is(err, sql.ErrNoRows) {
		t.Fatalf("boltless's squat row should not exist, got err=%v", err)
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times despite rejection", spy.creates)
	}
}

func TestIngestRepo_CreateHijackExistingRepoRejected(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "akshayskey", "did:plc:akshays-repo")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:boltless", "takeover", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	akshay := loadRepo(t, ing, "did:plc:akshay", "akshayskey")
	if akshay.Did != "did:plc:akshay" || akshay.Rkey != "akshayskey" {
		t.Errorf("akshay's row mutated: %+v", akshay)
	}
	if spy.renames != 0 {
		t.Errorf("RenameRepo called %d times despite rejection", spy.renames)
	}
}

func TestIngestRepo_CreateRenameIgnoresRkeyDrift(t *testing.T) {
	ing, spy := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "oldname", "oldrkey", "did:plc:akshays-repo")

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "newrkey", tangled.Repo{
		Knot:    "knot.example",
		Name:    ptr("newname"),
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	r := loadRepo(t, ing, "did:plc:akshay", "newrkey")
	if r.Name != "newname" {
		t.Errorf("rename did not apply despite matching owner: name=%q", r.Name)
	}
	if spy.renames != 1 {
		t.Errorf("RenameRepo called %d times, want 1", spy.renames)
	}
}

func TestIngestRepo_CreateVerifierTransientErrorPropagates(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{}, errors.New("knot unreachable")))

	err := ing.ingestRepo(context.Background(), e, ing.Logger)
	if err == nil {
		t.Fatalf("expected error on transient verifier failure, got nil")
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times despite verifier error", spy.creates)
	}
}

func TestIngestRepo_UpdateRejectsOwnerMismatch(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "akshayskey", "did:plc:akshays-repo")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:boltless", "akshayskey", tangled.Repo{
		Knot:        "knot.example",
		Description: ptr("boltless hijacks metadata"),
		RepoDid:     ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	akshay := loadRepo(t, ing, "did:plc:akshay", "akshayskey")
	if akshay.Description == "boltless hijacks metadata" {
		t.Errorf("update by non-owner applied: %+v", akshay)
	}
}

func TestIngestRepo_CreateInvalidRepoDidRejected(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:"),
	})

	verifierCalled := false
	withVerifier(ing, func(_ context.Context, _ repoverify.RepoDid) (repoverify.Result, error) {
		verifierCalled = true
		return repoverify.Result{}, nil
	})

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	if verifierCalled {
		t.Errorf("verifier was called with an invalid repoDid")
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times despite invalid repoDid", spy.creates)
	}
}

func TestIngestRepo_NilVerifierFailsClosed(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "knot.example",
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	err := ing.ingestRepo(context.Background(), e, ing.Logger)
	if err == nil {
		t.Fatalf("expected error when Verifier is nil, got nil")
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times despite nil verifier", spy.creates)
	}
}

func TestIngestRepo_CreateRejectsKnotMismatch(t *testing.T) {
	ing, spy := newTestIngester(t)

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "myrepo", tangled.Repo{
		Knot:    "evil.example",
		RepoDid: ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	if _, err := db.GetRepo(ing.Db,
		orm.FilterEq("did", "did:plc:akshay"),
		orm.FilterEq("rkey", "myrepo"),
	); !errors.Is(err, sql.ErrNoRows) {
		t.Fatalf("row should not be created for spoofed knot, err=%v", err)
	}
	if spy.creates != 0 {
		t.Errorf("NewRepo called %d times despite knot mismatch", spy.creates)
	}
}

func TestIngestRepo_UpdateRejectsKnotMismatch(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "akshayskey", "did:plc:akshays-repo")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "akshayskey", tangled.Repo{
		Knot:        "evil.example",
		Description: ptr("redirected clone target"),
		RepoDid:     ptr("did:plc:akshays-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:akshays-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	akshay := loadRepo(t, ing, "did:plc:akshay", "akshayskey")
	if akshay.Description == "redirected clone target" {
		t.Errorf("update with spoofed knot applied: %+v", akshay)
	}
	if akshay.Knot != "knot.example" {
		t.Errorf("row knot mutated to %q, want knot.example", akshay.Knot)
	}
}

func TestIngestRepo_UpdateRejectsRepoDidMutation(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "myrepo", "akshayskey", "did:plc:akshays-repo")

	e := makeEvent(t, jmodels.CommitOperationUpdate, "did:plc:akshay", "akshayskey", tangled.Repo{
		Knot:        "knot.example",
		Description: ptr("sneaky repoDid swap"),
		RepoDid:     ptr("did:plc:other-repo"),
	})

	withVerifier(ing, stubVerifier(repoverify.Result{
		RepoDid:  "did:plc:other-repo",
		OwnerDid: "did:plc:akshay",
		KnotURL:  mustKnotURL(t, "https://knot.example"),
	}, nil))

	if err := ing.ingestRepo(context.Background(), e, ing.Logger); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}
	akshay := loadRepo(t, ing, "did:plc:akshay", "akshayskey")
	if akshay.RepoDid != "did:plc:akshays-repo" {
		t.Errorf("repoDid mutated to %q, want did:plc:akshays-repo", akshay.RepoDid)
	}
	if akshay.Description == "sneaky repoDid swap" {
		t.Errorf("metadata from repoDid-mutating update applied: %+v", akshay)
	}
}

func renameAliasExists(t *testing.T, ing *Ingester, ownerDid, oldRkey string) bool {
	t.Helper()
	var n int
	if err := ing.Db.QueryRow(
		`select count(*) from repo_renames where owner_did = ? and old_rkey = ?`,
		ownerDid, oldRkey,
	).Scan(&n); err != nil {
		t.Fatalf("count repo_renames %q: %v", oldRkey, err)
	}
	return n > 0
}

func TestIngestRepo_RenameClearsCollidingAlias(t *testing.T) {
	ing, _ := newTestIngester(t)
	seedRepoRow(t, ing, "did:plc:akshay", "knot.example", "anemone-old", "anemone-old", "did:plc:anemone")
	if err := db.RecordRepoRename(ing.Db, "did:plc:akshay", "anemone", "did:plc:anemone"); err != nil {
		t.Fatalf("RecordRepoRename: %v", err)
	}

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "3mpxmsvicr2zn", tangled.Repo{
		Knot: "knot.example", Name: ptr("anemone"), RepoDid: ptr("did:plc:anemone"),
	})
	if err := ingestAcceptingOwner(t, ing, e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	if renameAliasExists(t, ing, "did:plc:akshay", "anemone") {
		t.Error("alias equal to the new live slug must be cleared to avoid a self-redirect loop")
	}
	if !renameAliasExists(t, ing, "did:plc:akshay", "anemone-old") {
		t.Error("alias for the prior slug must be recorded and survive")
	}
}

func TestIngestRepo_InsertClearsCollidingAlias(t *testing.T) {
	ing, _ := newTestIngester(t)
	if err := db.RecordRepoRename(ing.Db, "did:plc:akshay", "clam", "did:plc:clams-former-repo"); err != nil {
		t.Fatalf("RecordRepoRename: %v", err)
	}

	e := makeEvent(t, jmodels.CommitOperationCreate, "did:plc:akshay", "3mpxmfgowwck3", tangled.Repo{
		Knot: "knot.example", Name: ptr("clam"), RepoDid: ptr("did:plc:clam-repo"),
	})
	if err := ingestAcceptingOwner(t, ing, e); err != nil {
		t.Fatalf("ingestRepo: %v", err)
	}

	if renameAliasExists(t, ing, "did:plc:akshay", "clam") {
		t.Error("stale alias must be cleared when a live repo claims that slug")
	}
}
