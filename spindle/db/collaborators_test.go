package db

import (
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func subjectsOf(t *testing.T, d *DB, repoDid syntax.DID) []syntax.DID {
	t.Helper()
	rows, err := d.ListCollaboratorsByRepoDid(repoDid)
	if err != nil {
		t.Fatalf("ListCollaboratorsByRepoDid: %v", err)
	}
	out := make([]syntax.DID, 0, len(rows))
	for _, r := range rows {
		out = append(out, r.Subject)
	}
	return out
}

func TestAddKnotCollaborator_PersistsAndIsIdempotent(t *testing.T) {
	d := newTestDB(t)
	repo := syntax.DID("did:plc:repo")
	bob := syntax.DID("did:plc:bob")

	if err := d.AddKnotCollaborator(repo, bob); err != nil {
		t.Fatalf("add: %v", err)
	}
	if err := d.AddKnotCollaborator(repo, bob); err != nil {
		t.Fatalf("re-add: %v", err)
	}

	got := subjectsOf(t, d, repo)
	if len(got) != 1 || got[0] != bob {
		t.Fatalf("collaborators = %v, want exactly [bob]", got)
	}
}

func TestDeleteRepoCollaboratorBySubjectRepo(t *testing.T) {
	d := newTestDB(t)
	repo := syntax.DID("did:plc:repo")
	bob := syntax.DID("did:plc:bob")
	carol := syntax.DID("did:plc:carol")

	if err := d.AddKnotCollaborator(repo, bob); err != nil {
		t.Fatalf("add bob: %v", err)
	}
	if err := d.AddKnotCollaborator(repo, carol); err != nil {
		t.Fatalf("add carol: %v", err)
	}

	if err := d.DeleteRepoCollaboratorBySubjectRepo(bob, repo); err != nil {
		t.Fatalf("delete bob: %v", err)
	}
	got := subjectsOf(t, d, repo)
	if len(got) != 1 || got[0] != carol {
		t.Fatalf("after removing bob, collaborators = %v, want [carol]", got)
	}

	if err := d.DeleteRepoCollaboratorBySubjectRepo(bob, repo); err != nil {
		t.Fatalf("idempotent delete: %v", err)
	}
}

func TestKnotCollaborator_NoCollisionAcrossReposAndSubjects(t *testing.T) {
	d := newTestDB(t)
	repoA := syntax.DID("did:plc:repoA")
	repoB := syntax.DID("did:plc:repoB")
	bob := syntax.DID("did:plc:bob")
	carol := syntax.DID("did:plc:carol")

	for _, c := range []struct{ repo, subj syntax.DID }{
		{repoA, bob}, {repoB, bob}, {repoA, carol},
	} {
		if err := d.AddKnotCollaborator(c.repo, c.subj); err != nil {
			t.Fatalf("add %s/%s: %v", c.repo, c.subj, err)
		}
	}

	if got := subjectsOf(t, d, repoA); len(got) != 2 {
		t.Errorf("repoA collaborators = %v, want bob+carol", got)
	}
	if got := subjectsOf(t, d, repoB); len(got) != 1 || got[0] != bob {
		t.Errorf("repoB collaborators = %v, want [bob]", got)
	}

	if err := d.DeleteRepoCollaboratorBySubjectRepo(bob, repoA); err != nil {
		t.Fatalf("delete bob@repoA: %v", err)
	}
	if got := subjectsOf(t, d, repoB); len(got) != 1 || got[0] != bob {
		t.Errorf("repoB after removing bob@repoA = %v, want still [bob]", got)
	}
}
