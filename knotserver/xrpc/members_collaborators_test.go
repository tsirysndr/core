package xrpc

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/rbac"
)

const (
	aclOwner   = "did:plc:akshay"
	aclSubject = "did:plc:boltless"
	aclRepoDid = "did:plc:limpet"
)

type fakeIngester struct {
	added   []string
	removed []string
}

func (f *fakeIngester) AddDid(did string)    { f.added = append(f.added, did) }
func (f *fakeIngester) RemoveDid(did string) { f.removed = append(f.removed, did) }

func newACLXrpc(t *testing.T) (*Xrpc, *fakeIngester) {
	t.Helper()
	dir := t.TempDir()
	d, err := db.Setup(context.Background(), filepath.Join(dir, "knot.db"))
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	e, err := rbac.NewEnforcer(filepath.Join(dir, "rbac.db"))
	if err != nil {
		t.Fatalf("NewEnforcer: %v", err)
	}
	if err := e.AddKnot(rbac.ThisServer); err != nil {
		t.Fatalf("AddKnot: %v", err)
	}
	if err := e.AddKnotOwner(rbac.ThisServer, aclOwner); err != nil {
		t.Fatalf("AddKnotOwner: %v", err)
	}
	ing := &fakeIngester{}
	x := &Xrpc{
		Db:       d,
		Enforcer: e,
		Ingester: ing,
		Resolver: idresolver.DefaultResolver("http://127.0.0.1:1"),
		Config:   &config.Config{Server: config.Server{Hostname: "knot.example", MaxResponseKB: 5120}},
		Logger:   slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	return x, ing
}

func aclRequest(t *testing.T, actor string, body any) *http.Request {
	t.Helper()
	var buf bytes.Buffer
	if body != nil {
		if err := json.NewEncoder(&buf).Encode(body); err != nil {
			t.Fatalf("encode body: %v", err)
		}
	}
	req := httptest.NewRequest(http.MethodPost, "/xrpc/test", &buf)
	if actor != "" {
		req = req.WithContext(context.WithValue(req.Context(), ActorDid, syntax.DID(actor)))
	}
	return req
}

func seedRepo(t *testing.T, x *Xrpc) {
	t.Helper()
	if err := x.Db.StoreRepoKey(aclRepoDid, []byte("signing"), aclOwner, "reponame"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := x.Enforcer.AddRepo(aclOwner, rbac.ThisServer, aclRepoDid); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
}

func TestAddMember_HappyPath(t *testing.T) {
	x, ing := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if n, err := db.CountKnotMembersBySubject(x.Db, aclSubject); err != nil || n != 1 {
		t.Errorf("member rows = %d (err %v), want 1", n, err)
	}
	isM, err := x.Enforcer.IsKnotMember(aclSubject, rbac.ThisServer)
	if err != nil || !isM {
		t.Errorf("IsKnotMember = %v (err %v), want true", isM, err)
	}
	if len(ing.added) != 1 || ing.added[0] != aclSubject {
		t.Errorf("ingester.added = %v, want [%s]", ing.added, aclSubject)
	}
}

func TestAddMember_AlreadyMemberShortCircuits(t *testing.T) {
	x, ing := newACLXrpc(t)
	for i := 0; i < 2; i++ {
		rec := httptest.NewRecorder()
		x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
		if rec.Code != http.StatusOK {
			t.Fatalf("add %d status = %d, want 200", i, rec.Code)
		}
	}
	if n, _ := db.CountKnotMembersBySubject(x.Db, aclSubject); n != 1 {
		t.Errorf("member rows = %d, want 1 after duplicate add", n)
	}
	if len(ing.added) != 1 {
		t.Errorf("ingester.added = %v, want a single entry; second add must short-circuit", ing.added)
	}
}

func TestAddMember_MissingActorForbidden(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, "", tangled.KnotAddMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusForbidden {
		t.Errorf("status = %d, want 403", rec.Code)
	}
}

func TestAddMember_NonOwnerForbidden(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, aclSubject, tangled.KnotAddMember_Input{Subject: "did:plc:scallop"}))
	if rec.Code != http.StatusForbidden {
		t.Errorf("status = %d, want 403", rec.Code)
	}
}

func TestAddMember_MalformedSubjectBadRequest(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: "notadid"}))
	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}

func TestRemoveMember_HappyPathClearsBothStores(t *testing.T) {
	x, ing := newACLXrpc(t)
	addRec := httptest.NewRecorder()
	x.AddMember(addRec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
	if addRec.Code != http.StatusOK {
		t.Fatalf("setup add status = %d", addRec.Code)
	}

	rec := httptest.NewRecorder()
	x.RemoveMember(rec, aclRequest(t, aclOwner, tangled.KnotRemoveMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if n, _ := db.CountKnotMembersBySubject(x.Db, aclSubject); n != 0 {
		t.Errorf("member rows = %d, want 0", n)
	}
	if isM, _ := x.Enforcer.IsKnotMember(aclSubject, rbac.ThisServer); isM {
		t.Error("IsKnotMember still true after remove")
	}
	if len(ing.removed) != 1 || ing.removed[0] != aclSubject {
		t.Errorf("ingester.removed = %v, want [%s]", ing.removed, aclSubject)
	}
}

func TestRemoveMember_KeepsDidWhenOtherPolicyExists(t *testing.T) {
	x, ing := newACLXrpc(t)
	addRec := httptest.NewRecorder()
	x.AddMember(addRec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
	if addRec.Code != http.StatusOK {
		t.Fatalf("setup add status = %d", addRec.Code)
	}
	if err := x.Enforcer.AddRepo(aclSubject, rbac.ThisServer, aclRepoDid); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}

	rec := httptest.NewRecorder()
	x.RemoveMember(rec, aclRequest(t, aclOwner, tangled.KnotRemoveMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}
	if len(ing.removed) != 0 {
		t.Errorf("ingester.removed = %v, want empty; did still has repo policy", ing.removed)
	}
}

func TestRemoveMember_NonMemberNoop(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.RemoveMember(rec, aclRequest(t, aclOwner, tangled.KnotRemoveMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if isM, _ := x.Enforcer.IsKnotMember(aclSubject, rbac.ThisServer); isM {
		t.Error("non-member became a member after remove no-op")
	}
}

func TestRemoveCollaborator_NonCollaboratorNoop(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)
	rec := httptest.NewRecorder()
	x.RemoveCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if isC, _ := x.Enforcer.IsRepoCollaborator(aclSubject, rbac.ThisServer, aclRepoDid); isC {
		t.Error("non-collaborator became a collaborator after remove no-op")
	}
}

func TestAddCollaborator_HappyPath(t *testing.T) {
	x, ing := newACLXrpc(t)
	seedRepo(t, x)

	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	list, _, err := db.ListCollaborators(x.Db, syntax.DID(aclRepoDid), db.ListPage{Limit: db.ListMaxLimit})
	if err != nil || len(list) != 1 || list[0].Subject != syntax.DID(aclSubject) {
		t.Errorf("collaborators = %+v (err %v), want one boltless", list, err)
	}
	if isC, _ := x.Enforcer.IsRepoCollaborator(aclSubject, rbac.ThisServer, aclRepoDid); !isC {
		t.Error("IsRepoCollaborator false after add")
	}
	if len(ing.added) != 1 || ing.added[0] != aclSubject {
		t.Errorf("ingester.added = %v", ing.added)
	}
}

func TestAddCollaborator_MalformedRepoBadRequest(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)
	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: "notadid", Subject: aclSubject}))
	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}

func TestAddCollaborator_UnknownRepoNotFound(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: "did:plc:scallop", Subject: aclSubject}))
	if rec.Code != http.StatusNotFound {
		t.Errorf("status = %d, want 404", rec.Code)
	}
}

func TestAddCollaborator_NoInvitePermissionForbidden(t *testing.T) {
	x, _ := newACLXrpc(t)
	if err := x.Db.StoreRepoKey(aclRepoDid, []byte("signing"), aclOwner, "reponame"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rec.Code != http.StatusForbidden {
		t.Errorf("status = %d, want 403", rec.Code)
	}
}

func TestRemoveCollaborator_HappyPathClearsBothStores(t *testing.T) {
	x, ing := newACLXrpc(t)
	seedRepo(t, x)
	addRec := httptest.NewRecorder()
	x.AddCollaborator(addRec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if addRec.Code != http.StatusOK {
		t.Fatalf("setup add status = %d", addRec.Code)
	}

	rec := httptest.NewRecorder()
	x.RemoveCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if list, _, _ := db.ListCollaborators(x.Db, syntax.DID(aclRepoDid), db.ListPage{Limit: db.ListMaxLimit}); len(list) != 0 {
		t.Errorf("collaborators = %d, want 0", len(list))
	}
	if isC, _ := x.Enforcer.IsRepoCollaborator(aclSubject, rbac.ThisServer, aclRepoDid); isC {
		t.Error("IsRepoCollaborator still true after remove")
	}
	if len(ing.removed) != 1 || ing.removed[0] != aclSubject {
		t.Errorf("ingester.removed = %v", ing.removed)
	}
}

func TestRemoveCollaborator_MalformedRepoBadRequest(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.RemoveCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: "notadid", Subject: aclSubject}))
	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}

func TestRemoveCollaborator_UnknownRepoNotFound(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.RemoveCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: "did:plc:scallop", Subject: aclSubject}))
	if rec.Code != http.StatusNotFound {
		t.Errorf("status = %d, want 404", rec.Code)
	}
}

func TestRemoveCollaborator_OwnerKeepsRepoRights(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)

	if ok, _ := x.Enforcer.IsSettingsAllowed(aclOwner, rbac.ThisServer, aclRepoDid); !ok {
		t.Fatal("precondition: owner lacks repo:settings")
	}
	if ok, _ := x.Enforcer.IsPushAllowed(aclOwner, rbac.ThisServer, aclRepoDid); !ok {
		t.Fatal("precondition: owner lacks repo:push")
	}

	rec := httptest.NewRecorder()
	x.RemoveCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: aclRepoDid, Subject: aclOwner}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if ok, _ := x.Enforcer.IsSettingsAllowed(aclOwner, rbac.ThisServer, aclRepoDid); !ok {
		t.Error("owner lost repo:settings after removeCollaborator(owner)")
	}
	if ok, _ := x.Enforcer.IsPushAllowed(aclOwner, rbac.ThisServer, aclRepoDid); !ok {
		t.Error("owner lost repo:push after removeCollaborator(owner)")
	}
	if ok, _ := x.Enforcer.IsRepoOwner(aclOwner, rbac.ThisServer, aclRepoDid); !ok {
		t.Error("owner lost repo:owner after removeCollaborator(owner)")
	}
}

func TestAddCollaborator_OwnerIsNoOp(t *testing.T) {
	x, ing := newACLXrpc(t)
	seedRepo(t, x)

	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclOwner}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if list, _, _ := db.ListCollaborators(x.Db, syntax.DID(aclRepoDid), db.ListPage{Limit: db.ListMaxLimit}); len(list) != 0 {
		t.Errorf("collaborators = %d, want 0; owner must not become a redundant collaborator row", len(list))
	}
	if len(ing.added) != 0 {
		t.Errorf("ingester.added = %v, want empty for an owner no-op", ing.added)
	}
}

func TestRemoveMember_OwnerRejected(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.RemoveMember(rec, aclRequest(t, aclOwner, tangled.KnotRemoveMember_Input{Subject: aclOwner}))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400 for removing the owner", rec.Code)
	}
	if isOwner, _ := x.Enforcer.IsKnotOwner(aclOwner, rbac.ThisServer); !isOwner {
		t.Error("owner demoted by removeMember")
	}
	if isM, _ := x.Enforcer.IsKnotMember(aclOwner, rbac.ThisServer); !isM {
		t.Error("owner lost membership after removeMember")
	}
}

func TestAddMember_HealsCasbinOnlyDrift(t *testing.T) {
	x, _ := newACLXrpc(t)
	if _, err := x.Enforcer.TryAddKnotMember(rbac.ThisServer, aclSubject); err != nil {
		t.Fatalf("seed casbin-only membership: %v", err)
	}
	if n, _ := db.CountKnotMembersBySubject(x.Db, aclSubject); n != 0 {
		t.Fatalf("precondition: table already has %d rows", n)
	}

	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if n, _ := db.CountKnotMembersBySubject(x.Db, aclSubject); n != 1 {
		t.Errorf("member rows = %d, want 1; canonical table must be healed", n)
	}
}

func TestAddCollaborator_HealsCasbinOnlyDrift(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)
	if err := x.Enforcer.AddCollaborator(aclSubject, rbac.ThisServer, aclRepoDid); err != nil {
		t.Fatalf("seed casbin-only collaborator: %v", err)
	}
	if list, _, _ := db.ListCollaborators(x.Db, syntax.DID(aclRepoDid), db.ListPage{Limit: db.ListMaxLimit}); len(list) != 0 {
		t.Fatalf("precondition: table already has %d rows", len(list))
	}

	rec := httptest.NewRecorder()
	x.AddCollaborator(rec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if list, _, _ := db.ListCollaborators(x.Db, syntax.DID(aclRepoDid), db.ListPage{Limit: db.ListMaxLimit}); len(list) != 1 {
		t.Errorf("collaborators = %d, want 1; canonical table must be healed", len(list))
	}
}

func adminAddReq(t *testing.T, user, pass, subject string) *http.Request {
	t.Helper()
	var buf bytes.Buffer
	if err := json.NewEncoder(&buf).Encode(tangled.KnotAddMember_Input{Subject: subject}); err != nil {
		t.Fatalf("encode body: %v", err)
	}
	req := httptest.NewRequest(http.MethodPost, "/addMember", &buf)
	if user != "" || pass != "" {
		req.SetBasicAuth(user, pass)
	}
	return req
}

func TestAddMemberAdmin_RejectsBadCredentials(t *testing.T) {
	x, _ := newACLXrpc(t)
	x.Config.Server.AdminSecret = "hunter2"
	x.Config.Server.Owner = aclOwner
	srv := x.AdminRouter()

	cases := []struct {
		name string
		user string
		pass string
	}{
		{"no credentials", "", ""},
		{"wrong secret", "admin", "nope"},
		{"wrong user", "root", "hunter2"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			rec := httptest.NewRecorder()
			srv.ServeHTTP(rec, adminAddReq(t, c.user, c.pass, "did:plc:whelk"))
			if rec.Code != http.StatusUnauthorized {
				t.Fatalf("status = %d, want 401", rec.Code)
			}
			if n, _ := db.CountKnotMembersBySubject(x.Db, "did:plc:whelk"); n != 0 {
				t.Errorf("member rows = %d, want 0; bad credentials must not add", n)
			}
		})
	}
}

func TestAddMemberAdmin_RejectsWhenSecretUnset(t *testing.T) {
	x, _ := newACLXrpc(t)
	x.Config.Server.Owner = aclOwner
	srv := x.AdminRouter()

	rec := httptest.NewRecorder()
	srv.ServeHTTP(rec, adminAddReq(t, "admin", "hunter2", "did:plc:whelk"))
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("status = %d, want 401; admin api must stay closed with no secret configured", rec.Code)
	}
}

func TestAddMemberAdmin_AddsMember(t *testing.T) {
	x, ing := newACLXrpc(t)
	x.Config.Server.AdminSecret = "hunter2"
	x.Config.Server.Owner = aclOwner
	srv := x.AdminRouter()

	rec := httptest.NewRecorder()
	srv.ServeHTTP(rec, adminAddReq(t, "admin", "hunter2", "did:plc:whelk"))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if n, _ := db.CountKnotMembersBySubject(x.Db, "did:plc:whelk"); n != 1 {
		t.Errorf("member rows = %d, want 1 after admin add", n)
	}
	if isM, _ := x.Enforcer.IsKnotMember("did:plc:whelk", rbac.ThisServer); !isM {
		t.Error("IsKnotMember false after admin add")
	}
	if len(ing.added) != 1 || ing.added[0] != "did:plc:whelk" {
		t.Errorf("ingester.added = %v, want [did:plc:whelk]", ing.added)
	}
}
