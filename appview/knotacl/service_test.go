package knotacl

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/consts"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
)

var capsKnotACL = []string{string(consts.CapKnotACL)}

type fakeKnot struct {
	version       string
	capabilities  []string
	members       []string
	collaborators []string
	listStatus    int

	mu       sync.Mutex
	listHits int
}

func (k *fakeKnot) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	switch {
	case strings.HasSuffix(r.URL.Path, tangled.KnotVersionNSID):
		if k.version == "" {
			http.Error(w, "version down", http.StatusInternalServerError)
			return
		}
		json.NewEncoder(w).Encode(tangled.KnotVersion_Output{Version: k.version, Capabilities: k.capabilities})
	case strings.HasSuffix(r.URL.Path, tangled.KnotListMembersNSID):
		k.hit()
		if k.listStatus != 0 {
			http.Error(w, "list down", k.listStatus)
			return
		}
		json.NewEncoder(w).Encode(memberPage(k.members, ""))
	case strings.HasSuffix(r.URL.Path, tangled.RepoListCollaboratorsNSID):
		k.hit()
		if k.listStatus != 0 {
			http.Error(w, "list down", k.listStatus)
			return
		}
		json.NewEncoder(w).Encode(collabPage(k.collaborators, ""))
	default:
		http.NotFound(w, r)
	}
}

func (k *fakeKnot) hit() {
	k.mu.Lock()
	k.listHits++
	k.mu.Unlock()
}

func (k *fakeKnot) hits() int {
	k.mu.Lock()
	defer k.mu.Unlock()
	return k.listHits
}

func newServiceEnv(t *testing.T, knot *fakeKnot, seed func(e *rbac.Enforcer, host string)) (*Service, *db.DB, string) {
	t.Helper()
	srv := httptest.NewServer(knot)
	t.Cleanup(srv.Close)
	host := strings.TrimPrefix(srv.URL, "http://")

	dir := t.TempDir()
	enforcer, err := rbac.NewEnforcer(filepath.Join(dir, "rbac.db"))
	if err != nil {
		t.Fatalf("NewEnforcer: %v", err)
	}
	if err := enforcer.AddKnot(host); err != nil {
		t.Fatalf("AddKnot: %v", err)
	}
	if err := enforcer.AddKnotOwner(host, testOwner); err != nil {
		t.Fatalf("AddKnotOwner: %v", err)
	}
	if seed != nil {
		seed(enforcer, host)
	}

	d, err := db.Make(context.Background(), filepath.Join(dir, "appview.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}

	return NewService(enforcer, d, true, testLogger()), d, host
}

func testRepo(host string) *models.Repo {
	return &models.Repo{Did: testOwner, Knot: host, RepoDid: testRepoDid, Name: "anemone"}
}

func seedRepoPolicies(t *testing.T, e *rbac.Enforcer, host string) {
	t.Helper()
	if err := e.AddRepo(testOwner, host, testRepoDid); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := e.AddCollaborator(testCollab, host, testRepoDid); err != nil {
		t.Fatalf("AddCollaborator: %v", err)
	}
}

func sortedRoles(roles []string) []string {
	s := slices.Clone(roles)
	slices.Sort(s)
	return slices.Compact(s)
}

func TestService_OldKnotUsesCasbinNoLiveQuery(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: "v1.14.0"}
	svc, _, host := newServiceEnv(t, knot, func(e *rbac.Enforcer, h string) { seedRepoPolicies(t, e, h) })

	collab := svc.RolesInRepo(ctx, testRepo(host), testCollab)
	if !collab.IsCollaborator() || !collab.IsPushAllowed() {
		t.Errorf("collaborator roles = %v, want collaborator+push from casbin", collab.Roles)
	}
	if !svc.HasRepoPermission(ctx, testRepo(host), testOwner, "repo:owner") {
		t.Error("owner should hold repo:owner via casbin")
	}
	if knot.hits() != 0 {
		t.Errorf("listHits = %d, want 0; an old knot must never be live-queried", knot.hits())
	}
}

func TestService_ParityOldVsNew(t *testing.T) {
	ctx := context.Background()
	oldSvc, _, oldHost := newServiceEnv(t, &fakeKnot{version: "v1.14.0"}, func(e *rbac.Enforcer, h string) { seedRepoPolicies(t, e, h) })
	newSvc, _, newHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, collaborators: []string{testCollab}}, nil)

	for _, did := range []string{testOwner, testCollab} {
		oldRoles := sortedRoles(oldSvc.RolesInRepo(ctx, testRepo(oldHost), did).Roles)
		newRoles := sortedRoles(newSvc.RolesInRepo(ctx, testRepo(newHost), did).Roles)
		if !slices.Equal(oldRoles, newRoles) {
			t.Errorf("did %s: casbin roles %v != synth roles %v; synthesis has drifted from the policy grants", did, oldRoles, newRoles)
		}
	}
}

func TestService_NewKnotOwnerFromRecord(t *testing.T) {
	ctx := context.Background()
	svc, _, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL}, nil)

	owner := svc.RolesInRepo(ctx, testRepo(host), testOwner)
	if !owner.IsOwner() || !owner.IsPushAllowed() || !owner.SettingsAllowed() || !owner.RepoDeleteAllowed() {
		t.Errorf("owner roles = %v, want the full owner set derived from repo.Did", owner.Roles)
	}
	if stranger := svc.RolesInRepo(ctx, testRepo(host), testStrange); len(stranger.Roles) != 0 {
		t.Errorf("stranger roles = %v, want empty", stranger.Roles)
	}
}

func TestService_NewKnotCollaboratorFromList(t *testing.T) {
	ctx := context.Background()
	svc, _, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, collaborators: []string{testCollab}}, nil)

	collab := svc.RolesInRepo(ctx, testRepo(host), testCollab)
	if !collab.IsCollaborator() || !collab.IsPushAllowed() {
		t.Errorf("collaborator roles = %v, want collaborator+push from the live list", collab.Roles)
	}
	if collab.IsOwner() || collab.RepoDeleteAllowed() {
		t.Errorf("collaborator must not hold owner/delete: %v", collab.Roles)
	}
}

func TestService_MixedFleet(t *testing.T) {
	ctx := context.Background()
	oldSvc, _, oldHost := newServiceEnv(t, &fakeKnot{version: "v1.14.0"}, func(e *rbac.Enforcer, h string) { seedRepoPolicies(t, e, h) })
	newSvc, _, newHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, collaborators: []string{testCollab}}, nil)

	if !newSvc.RolesInRepo(ctx, testRepo(newHost), testCollab).IsCollaborator() {
		t.Error("new-knot collaborator must resolve from the live query with an empty casbin")
	}
	if !newSvc.RolesInRepo(ctx, testRepo(newHost), testOwner).IsOwner() {
		t.Error("new-knot owner must resolve from repo.Did")
	}
	if !oldSvc.RolesInRepo(ctx, testRepo(oldHost), testCollab).IsCollaborator() {
		t.Error("old-knot collaborator must resolve from casbin")
	}
}

func TestService_IsRepoCreateAllowed(t *testing.T) {
	ctx := context.Background()

	oldSvc, _, oldHost := newServiceEnv(t, &fakeKnot{version: "v1.14.0"}, func(e *rbac.Enforcer, h string) {
		if _, err := e.TryAddKnotMember(h, testCollab); err != nil {
			t.Fatalf("TryAddKnotMember: %v", err)
		}
	})
	if !oldSvc.IsRepoCreateAllowed(ctx, oldHost, testCollab) {
		t.Error("old-knot member should be allowed to create")
	}
	if oldSvc.IsRepoCreateAllowed(ctx, oldHost, testStrange) {
		t.Error("old-knot non-member should not be allowed to create")
	}

	memberSvc, _, memberHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, members: []string{testCollab}}, nil)
	if !memberSvc.IsRepoCreateAllowed(ctx, memberHost, testCollab) {
		t.Error("new-knot listed member should be allowed to create")
	}

	ownerSvc, ownerDb, ownerHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL}, nil)
	if err := db.AddKnot(ownerDb, ownerHost, testOwner); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(ownerDb, orm.FilterEq("domain", ownerHost), orm.FilterEq("did", testOwner)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}
	if !ownerSvc.IsRepoCreateAllowed(ctx, ownerHost, testOwner) {
		t.Error("new-knot registered owner should be allowed to create even when absent from listMembers")
	}
	if ownerSvc.IsRepoCreateAllowed(ctx, ownerHost, testStrange) {
		t.Error("new-knot non-member non-owner should not be allowed to create")
	}
}

func TestService_KnotDownDegrades(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, listStatus: http.StatusInternalServerError}
	svc, _, host := newServiceEnv(t, knot, nil)
	repo := testRepo(host)

	if !svc.RolesInRepo(ctx, repo, testOwner).IsOwner() {
		t.Error("owner must still resolve from repo.Did when the knot list is down")
	}
	if roles := svc.RolesInRepo(ctx, repo, testCollab); len(roles.Roles) != 0 {
		t.Errorf("non-owner roles when knot down = %v, want empty (degrade, not error)", roles.Roles)
	}
	if collabs := svc.Collaborators(ctx, repo); len(collabs) != 1 || collabs[0].Did != testOwner || collabs[0].Role != "owner" {
		t.Errorf("Collaborators when knot down = %v, want only the owner row", collabs)
	}
	if m := svc.KnotMembers(ctx, host); m != nil {
		t.Errorf("KnotMembers when knot down = %v, want nil", m)
	}
	if svc.IsRepoCreateAllowed(ctx, host, testStrange) {
		t.Error("create gate must be false when the knot is down and the user is not a registered owner")
	}
}

func TestService_KnotOwnerForeignRepoParity(t *testing.T) {
	ctx := context.Background()
	foreignRepoDid := "did:plc:whelk"

	oldSvc, _, oldHost := newServiceEnv(t, &fakeKnot{version: "v1.14.0"}, func(e *rbac.Enforcer, h string) {
		if err := e.AddRepo(testCollab, h, foreignRepoDid); err != nil {
			t.Fatalf("AddRepo: %v", err)
		}
	})
	oldRepo := &models.Repo{Did: testCollab, Knot: oldHost, RepoDid: foreignRepoDid, Name: "barnacle"}
	oldRoles := sortedRoles(oldSvc.RolesInRepo(ctx, oldRepo, testOwner).Roles)

	newSvc, newDb, newHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL}, nil)
	if err := db.AddKnot(newDb, newHost, testOwner); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(newDb, orm.FilterEq("domain", newHost), orm.FilterEq("did", testOwner)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}
	newRepo := &models.Repo{Did: testCollab, Knot: newHost, RepoDid: foreignRepoDid, Name: "barnacle"}
	newRoles := sortedRoles(newSvc.RolesInRepo(ctx, newRepo, testOwner).Roles)

	if !slices.Equal(oldRoles, newRoles) {
		t.Errorf("knot operator on a member repo: old=%v new=%v; the appview gate must not diverge by knot version", oldRoles, newRoles)
	}
	if !slices.Contains(newRoles, "repo:delete") {
		t.Errorf("knot operator must retain repo:delete on a member repo, got %v", newRoles)
	}
	if newSvc.HasRepoPermission(ctx, newRepo, testStrange, "repo:delete") {
		t.Error("a stranger must not hold repo:delete on a foreign repo")
	}
}

func TestService_KnotMembersIncludesOwner(t *testing.T) {
	ctx := context.Background()
	svc, d, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, members: []string{testCollab}}, nil)
	if err := db.AddKnot(d, host, testOwner); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(d, orm.FilterEq("domain", host), orm.FilterEq("did", testOwner)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}

	members := svc.KnotMembers(ctx, host)
	if !slices.Contains(members, testOwner) {
		t.Errorf("new-knot roster %v must include the registered owner so the dashboard renders the owner's repos", members)
	}
	if !slices.Contains(members, testCollab) {
		t.Errorf("new-knot roster %v must include listed members", members)
	}
}

func TestService_CollaboratorsNewKnot(t *testing.T) {
	ctx := context.Background()

	svc, _, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, collaborators: []string{testCollab}}, nil)
	collabs := svc.Collaborators(ctx, testRepo(host))
	if len(collabs) != 2 {
		t.Fatalf("Collaborators = %v, want owner + one collaborator", collabs)
	}
	if collabs[0].Did != testOwner || collabs[0].Role != "owner" {
		t.Errorf("first row = %v, want the owner", collabs[0])
	}
	if collabs[1].Did != testCollab || collabs[1].Role != "collaborator" {
		t.Errorf("second row = %v, want the collaborator", collabs[1])
	}

	dupSvc, _, dupHost := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, collaborators: []string{testCollab, testOwner}}, nil)
	rows := dupSvc.Collaborators(ctx, testRepo(dupHost))
	ownerRows := 0
	for _, c := range rows {
		if c.Did == testOwner {
			ownerRows++
		}
	}
	if ownerRows != 1 {
		t.Errorf("owner listed %d times, want exactly the single owner row: %v", ownerRows, rows)
	}
}

func TestService_HasRepoPermissionErr_OwnerNeedsNoLiveQuery(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, listStatus: http.StatusInternalServerError}
	svc, _, host := newServiceEnv(t, knot, nil)

	ok, err := svc.HasRepoPermissionErr(ctx, testRepo(host), testOwner, "repo:push")
	if err != nil || !ok {
		t.Errorf("owner push = (%v, %v), want (true, nil) resolved from repo.Did with the list down", ok, err)
	}
	if knot.hits() != 0 {
		t.Errorf("listHits = %d, want 0; the owner must not trigger a live query", knot.hits())
	}
}

func TestService_HasRepoPermissionErr_KnotDownIsUndetermined(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, listStatus: http.StatusInternalServerError}
	svc, _, host := newServiceEnv(t, knot, nil)

	ok, err := svc.HasRepoPermissionErr(ctx, testRepo(host), testCollab, "repo:push")
	if !errors.Is(err, ErrKnotUnreachable) {
		t.Errorf("err = %v, want ErrKnotUnreachable so the ingester can fail open instead of dropping the record", err)
	}
	if ok {
		t.Error("ok must be false when the answer is undetermined")
	}
	if svc.HasRepoPermission(ctx, testRepo(host), testCollab, "repo:push") {
		t.Error("HasRepoPermission must fail closed when the knot is unreachable")
	}
}

func TestService_HasRepoPermissionErr_DefinitiveDenyIsNotUndetermined(t *testing.T) {
	ctx := context.Background()
	svc, _, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL}, nil)

	ok, err := svc.HasRepoPermissionErr(ctx, testRepo(host), testStrange, "repo:push")
	if err != nil {
		t.Errorf("err = %v, want nil; a reachable knot gives a definitive answer", err)
	}
	if ok {
		t.Error("a stranger must not hold push")
	}
}

func TestService_KnotMembersDegradesToOwner(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, listStatus: http.StatusInternalServerError}
	svc, d, host := newServiceEnv(t, knot, nil)
	if err := db.AddKnot(d, host, testOwner); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(d, orm.FilterEq("domain", host), orm.FilterEq("did", testOwner)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}

	members := svc.KnotMembers(ctx, host)
	if !slices.Contains(members, testOwner) {
		t.Errorf("KnotMembers with the list down = %v, want the registered owner so the dashboard still renders", members)
	}
}

func TestService_VersionDownFailsClosedToCasbin(t *testing.T) {
	ctx := context.Background()
	knot := &fakeKnot{version: ""}
	svc, _, host := newServiceEnv(t, knot, func(e *rbac.Enforcer, h string) { seedRepoPolicies(t, e, h) })

	if !svc.RolesInRepo(ctx, testRepo(host), testCollab).IsCollaborator() {
		t.Error("an unreachable version probe must fail closed to the casbin path, which knows the collaborator")
	}
	if knot.hits() != 0 {
		t.Errorf("listHits = %d; failing closed must route to casbin, never the live list", knot.hits())
	}
}

func TestService_RegisteredOwnersMemoizedPerRequest(t *testing.T) {
	svc, d, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL}, nil)
	if err := db.AddKnot(d, host, testOwner); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(d, orm.FilterEq("domain", host), orm.FilterEq("did", testOwner)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}

	ctx := WithMemo(context.Background())
	first := svc.nat.registeredOwners(ctx, host)
	if !slices.Contains(first, testOwner) {
		t.Fatalf("first read = %v, want testOwner", first)
	}

	if err := db.AddKnot(d, host, testCollab); err != nil {
		t.Fatalf("db.AddKnot: %v", err)
	}
	if err := db.MarkRegistered(d, orm.FilterEq("domain", host), orm.FilterEq("did", testCollab)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}

	if second := svc.nat.registeredOwners(ctx, host); slices.Contains(second, testCollab) {
		t.Errorf("second read in the same request saw a newly added owner %v; the memo did not short-circuit the DB", second)
	}
	if fresh := svc.nat.registeredOwners(context.Background(), host); !slices.Contains(fresh, testCollab) {
		t.Errorf("a fresh request %v must re-query and see the new owner", fresh)
	}

	first[0] = "did:plc:squid"
	if again := svc.nat.registeredOwners(ctx, host); slices.Contains(again, "did:plc:squid") {
		t.Errorf("mutating the returned slice corrupted the memo: %v", again)
	}
}
