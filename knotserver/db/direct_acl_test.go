package db

import (
	"context"
	"path/filepath"
	"slices"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func newACLTestDB(t *testing.T) *DB {
	t.Helper()
	d, err := Setup(context.Background(), filepath.Join(t.TempDir(), "knot.db"))
	if err != nil {
		t.Fatalf("Setup: %v", err)
	}
	return d
}

func countMembers(t *testing.T, d *DB, where string, args ...any) int {
	t.Helper()
	var n int
	if err := d.QueryRow("select count(1) from knot_members where "+where, args...).Scan(&n); err != nil {
		t.Fatalf("count members: %v", err)
	}
	return n
}

func seedLegacyMember(t *testing.T, d *DB, owner, rkey, subject string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into knot_members (did, rkey, subject) values (?, ?, ?)`,
		owner, rkey, subject,
	); err != nil {
		t.Fatalf("seed legacy member: %v", err)
	}
}

func TestAddKnotMemberDirect_IdempotentUnderPartialUnique(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")

	if err := AddKnotMemberDirect(d, owner, subject); err != nil {
		t.Fatalf("first add: %v", err)
	}
	if err := AddKnotMemberDirect(d, owner, subject); err != nil {
		t.Fatalf("second add: %v", err)
	}

	if got := countMembers(t, d, "subject = ? and rkey is null", subject); got != 1 {
		t.Errorf("direct rows = %d, want 1", got)
	}
}

func TestRemoveKnotMemberDirect_PreservesLegacyRow(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")

	seedLegacyMember(t, d, owner.String(), "legacy-rk", subject.String())
	if err := AddKnotMemberDirect(d, owner, subject); err != nil {
		t.Fatalf("direct add: %v", err)
	}
	if got := countMembers(t, d, "subject = ?", subject.String()); got != 2 {
		t.Fatalf("rows = %d, want 2 legacy plus direct", got)
	}

	if err := RemoveKnotMemberDirect(d, subject); err != nil {
		t.Fatalf("remove direct: %v", err)
	}
	if got := countMembers(t, d, "subject = ? and rkey is null", subject.String()); got != 0 {
		t.Errorf("direct rows after remove = %d, want 0", got)
	}
	if got := countMembers(t, d, "subject = ? and rkey = 'legacy-rk'", subject.String()); got != 1 {
		t.Errorf("legacy row = %d, want 1 preserved", got)
	}
}

func TestRemoveKnotMemberBySubject_RemovesAllRows(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")

	seedLegacyMember(t, d, owner.String(), "legacy-rk", subject.String())
	if err := AddKnotMemberDirect(d, owner, subject); err != nil {
		t.Fatalf("direct add: %v", err)
	}

	if err := RemoveKnotMemberBySubject(d, subject); err != nil {
		t.Fatalf("remove by subject: %v", err)
	}
	if got := countMembers(t, d, "subject = ?", subject.String()); got != 0 {
		t.Errorf("rows after remove = %d, want 0", got)
	}
}

func TestCollaborators_AddListRemoveScopedByRepo(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")
	repoA := syntax.DID("did:plc:limpet")
	repoB := syntax.DID("did:plc:scallop")

	for _, repo := range []syntax.DID{repoA, repoB} {
		if err := AddCollaborator(d, Collaborator{RepoDid: repo, Subject: subject, AddedBy: owner}); err != nil {
			t.Fatalf("add collaborator on %s: %v", repo, err)
		}
	}

	listA, _, err := ListCollaborators(d, repoA, ListPage{Limit: ListMaxLimit})
	if err != nil {
		t.Fatalf("list A: %v", err)
	}
	if len(listA) != 1 || listA[0].Subject != subject || listA[0].RepoDid != repoA {
		t.Fatalf("repoA collaborators = %+v, want one boltless on limpet", listA)
	}

	if err := RemoveCollaborator(d, repoA, subject); err != nil {
		t.Fatalf("remove on A: %v", err)
	}
	if listA, _, _ = ListCollaborators(d, repoA, ListPage{Limit: ListMaxLimit}); len(listA) != 0 {
		t.Errorf("repoA after remove = %d, want 0", len(listA))
	}
	if listB, _, _ := ListCollaborators(d, repoB, ListPage{Limit: ListMaxLimit}); len(listB) != 1 {
		t.Errorf("repoB after removing from A = %d, want 1 scoped", len(listB))
	}
}

func TestAddCollaborator_IdempotentUnderUnique(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")
	repo := syntax.DID("did:plc:limpet")

	if err := AddCollaborator(d, Collaborator{RepoDid: repo, Subject: subject, AddedBy: owner}); err != nil {
		t.Fatalf("first add: %v", err)
	}
	if err := AddCollaborator(d, Collaborator{RepoDid: repo, Subject: subject, AddedBy: owner}); err != nil {
		t.Fatalf("second add: %v", err)
	}

	list, _, err := ListCollaborators(d, repo, ListPage{Limit: ListMaxLimit})
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(list) != 1 {
		t.Errorf("rows = %d, want 1", len(list))
	}
}

func TestListKnotMembers_DedupsLegacyAndDirect(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")

	seedLegacyMember(t, d, owner.String(), "legacy-rk", subject.String())
	if err := AddKnotMemberDirect(d, owner, subject); err != nil {
		t.Fatalf("direct add: %v", err)
	}
	if got := countMembers(t, d, "subject = ?", subject.String()); got != 2 {
		t.Fatalf("raw rows = %d, want 2 (legacy + direct)", got)
	}

	members, next, err := ListKnotMembers(d, ListPage{Limit: ListMaxLimit})
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(members) != 1 {
		t.Fatalf("members = %d, want 1; legacy and direct rows for one subject must collapse", len(members))
	}
	if members[0].Subject != subject {
		t.Errorf("subject = %s, want %s", members[0].Subject, subject)
	}
	if next != nil {
		t.Errorf("cursor = %v, want nil for a complete page", *next)
	}
}

func TestListKnotMembers_OrderAndKeyset(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	for _, s := range []string{"did:plc:limpet", "did:plc:whelk", "did:plc:scallop"} {
		if err := AddKnotMemberDirect(d, owner, syntax.DID(s)); err != nil {
			t.Fatalf("add %s: %v", s, err)
		}
	}

	asc, _, err := ListKnotMembers(d, ListPage{Limit: ListMaxLimit, Desc: false})
	if err != nil {
		t.Fatalf("asc: %v", err)
	}
	if !slices.IsSortedFunc(asc, func(a, b KnotMember) int { return a.Id - b.Id }) {
		t.Errorf("asc not ascending by id: %+v", asc)
	}

	desc, _, err := ListKnotMembers(d, ListPage{Limit: ListMaxLimit, Desc: true})
	if err != nil {
		t.Fatalf("desc: %v", err)
	}
	if !slices.IsSortedFunc(desc, func(a, b KnotMember) int { return b.Id - a.Id }) {
		t.Errorf("desc not descending by id: %+v", desc)
	}

	page1, next, err := ListKnotMembers(d, ListPage{Limit: 2, Desc: false})
	if err != nil {
		t.Fatalf("page1: %v", err)
	}
	if len(page1) != 2 || next == nil {
		t.Fatalf("page1 = %d next=%v, want 2 rows and a cursor", len(page1), next)
	}
	page2, next2, err := ListKnotMembers(d, ListPage{Limit: 2, Cursor: next, Desc: false})
	if err != nil {
		t.Fatalf("page2: %v", err)
	}
	if len(page2) != 1 || next2 != nil {
		t.Fatalf("page2 = %d next=%v, want 1 row and no cursor", len(page2), next2)
	}
	if page1[len(page1)-1].Id >= page2[0].Id {
		t.Errorf("keyset overlap or gap: page1 last id %d, page2 first id %d", page1[len(page1)-1].Id, page2[0].Id)
	}
}

func TestListKnotMembers_ZeroValuePageDefaultsLimit(t *testing.T) {
	d := newACLTestDB(t)
	owner := syntax.DID("did:plc:akshay")
	for _, s := range []string{"did:plc:limpet", "did:plc:whelk"} {
		if err := AddKnotMemberDirect(d, owner, syntax.DID(s)); err != nil {
			t.Fatalf("add %s: %v", s, err)
		}
	}

	for _, p := range []ListPage{{}, {Limit: -3}} {
		members, next, err := ListKnotMembers(d, p)
		if err != nil {
			t.Fatalf("list with page %+v: %v", p, err)
		}
		if len(members) != 2 {
			t.Errorf("page %+v: members = %d, want 2 under the default limit", p, len(members))
		}
		if next != nil {
			t.Errorf("page %+v: cursor = %v, want nil", p, *next)
		}
	}
}

func TestDeleteRepoKeyRemovesCollaborators(t *testing.T) {
	d := newACLTestDB(t)

	repoDid := syntax.DID("did:plc:whelk")
	owner := syntax.DID("did:plc:akshay")
	subject := syntax.DID("did:plc:boltless")

	if err := d.StoreRepoKey(repoDid.String(), []byte("signing"), owner.String(), "reponame"); err != nil {
		t.Fatalf("StoreRepoKey: %v", err)
	}
	if err := AddCollaborator(d, Collaborator{RepoDid: repoDid, Subject: subject, AddedBy: owner}); err != nil {
		t.Fatalf("AddCollaborator: %v", err)
	}
	if ok, err := IsCollaborator(d, repoDid, subject); err != nil || !ok {
		t.Fatalf("collaborator missing before delete: ok=%v err=%v", ok, err)
	}

	if err := d.DeleteRepoKey(repoDid.String()); err != nil {
		t.Fatalf("DeleteRepoKey: %v", err)
	}

	if ok, err := IsCollaborator(d, repoDid, subject); err != nil || ok {
		t.Fatalf("collaborator not removed after repo delete: ok=%v err=%v", ok, err)
	}
}
