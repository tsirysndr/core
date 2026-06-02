package xrpc

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"

	"tangled.org/core/api/tangled"
)

func listRequest(t *testing.T, params url.Values) *http.Request {
	t.Helper()
	return httptest.NewRequest(http.MethodGet, "/xrpc/test?"+params.Encode(), nil)
}

func decodeMembers(t *testing.T, rec *httptest.ResponseRecorder) tangled.KnotListMembers_Output {
	t.Helper()
	var out tangled.KnotListMembers_Output
	if err := json.NewDecoder(rec.Body).Decode(&out); err != nil {
		t.Fatalf("decode members: %v", err)
	}
	return out
}

func seedMembers(t *testing.T, x *Xrpc, subjects ...string) {
	t.Helper()
	for _, s := range subjects {
		rec := httptest.NewRecorder()
		x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: s}))
		if rec.Code != http.StatusOK {
			t.Fatalf("seed member %s: status %d, body=%s", s, rec.Code, rec.Body.String())
		}
	}
}

func TestListMembers_ReturnsAddedSubjects(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedMembers(t, x, aclSubject, "did:plc:scallop")

	rec := httptest.NewRecorder()
	x.ListMembers(rec, listRequest(t, url.Values{"subject": {"knot.example"}}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}

	out := decodeMembers(t, rec)
	if len(out.Items) != 2 {
		t.Fatalf("items = %d, want 2", len(out.Items))
	}
	if out.Cursor != nil {
		t.Errorf("cursor = %q, want nil for a complete page", *out.Cursor)
	}

	for _, it := range out.Items {
		if it.AddedBy != aclOwner {
			t.Errorf("subject %s addedBy = %q, want %s", it.Subject, it.AddedBy, aclOwner)
		}
		if it.CreatedAt == "" {
			t.Errorf("subject %s has empty createdAt", it.Subject)
		}
		if it.Uri != nil || it.Cid != nil {
			t.Errorf("subject %s carries record uri/cid; a knot must omit them", it.Subject)
		}
	}
}

func TestListMembers_Empty(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.ListMembers(rec, listRequest(t, url.Values{"subject": {"knot.example"}}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}
	out := decodeMembers(t, rec)
	if len(out.Items) != 0 {
		t.Errorf("items = %d, want 0", len(out.Items))
	}
}

func TestListMembers_RejectsMalformedParams(t *testing.T) {
	x, _ := newACLXrpc(t)
	for _, q := range []url.Values{
		{"limit": {"abc"}},
		{"cursor": {"notanint"}},
		{"order": {"ascending"}},
	} {
		rec := httptest.NewRecorder()
		x.ListMembers(rec, listRequest(t, q))
		if rec.Code != http.StatusBadRequest {
			t.Errorf("params %v: status = %d, want 400", q, rec.Code)
		}
	}
}

func TestListMembers_ClampsOutOfRangeLimit(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedMembers(t, x, aclSubject)
	rec := httptest.NewRecorder()
	x.ListMembers(rec, listRequest(t, url.Values{"limit": {"5000"}}))
	if rec.Code != http.StatusOK {
		t.Fatalf("limit=5000 should clamp and return 200, got %d; body=%s", rec.Code, rec.Body.String())
	}
	if len(decodeMembers(t, rec).Items) != 1 {
		t.Error("clamped limit must still return the seeded member")
	}
}

func TestListMembers_PaginatesWithoutOverlap(t *testing.T) {
	x, _ := newACLXrpc(t)
	all := []string{"did:plc:scallop", "did:plc:whelk", "did:plc:limpet"}
	seedMembers(t, x, all...)

	seen := map[string]bool{}
	params := url.Values{"subject": {"knot.example"}, "limit": {"2"}}

	rec := httptest.NewRecorder()
	x.ListMembers(rec, listRequest(t, params))
	page1 := decodeMembers(t, rec)
	if len(page1.Items) != 2 || page1.Cursor == nil {
		t.Fatalf("page1 items=%d cursor=%v, want 2 items and a cursor", len(page1.Items), page1.Cursor)
	}
	for _, it := range page1.Items {
		seen[it.Subject] = true
	}

	params.Set("cursor", *page1.Cursor)
	rec = httptest.NewRecorder()
	x.ListMembers(rec, listRequest(t, params))
	page2 := decodeMembers(t, rec)
	if len(page2.Items) != 1 {
		t.Fatalf("page2 items = %d, want 1", len(page2.Items))
	}
	if page2.Cursor != nil {
		t.Errorf("page2 cursor = %q, want nil at end", *page2.Cursor)
	}
	for _, it := range page2.Items {
		if seen[it.Subject] {
			t.Errorf("subject %s repeated across pages", it.Subject)
		}
		seen[it.Subject] = true
	}

	if len(seen) != len(all) {
		t.Errorf("distinct subjects seen = %d, want %d", len(seen), len(all))
	}
}

func TestListCollaborators_ScopedToRepo(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)

	addRec := httptest.NewRecorder()
	x.AddCollaborator(addRec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if addRec.Code != http.StatusOK {
		t.Fatalf("seed collaborator: status %d, body=%s", addRec.Code, addRec.Body.String())
	}

	rec := httptest.NewRecorder()
	x.ListCollaborators(rec, listRequest(t, url.Values{"subject": {aclRepoDid}}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}

	var out tangled.RepoListCollaborators_Output
	if err := json.NewDecoder(rec.Body).Decode(&out); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if len(out.Items) != 1 {
		t.Fatalf("items = %d, want 1", len(out.Items))
	}
	if out.Items[0].Subject != aclSubject {
		t.Errorf("subject = %q, want %s", out.Items[0].Subject, aclSubject)
	}
	if out.Items[0].AddedBy != aclOwner {
		t.Errorf("addedBy = %q, want %s", out.Items[0].AddedBy, aclOwner)
	}
	if out.Items[0].Uri != nil || out.Items[0].Cid != nil {
		t.Error("collaborator carries record uri/cid; a knot must omit them")
	}
}

func TestListCollaborators_MalformedSubjectBadRequest(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.ListCollaborators(rec, listRequest(t, url.Values{"subject": {"notadid"}}))
	if rec.Code != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", rec.Code)
	}
}

func TestListCollaborators_UnknownRepoEmpty(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.ListCollaborators(rec, listRequest(t, url.Values{"subject": {"did:plc:scallop"}}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}
	var out tangled.RepoListCollaborators_Output
	if err := json.NewDecoder(rec.Body).Decode(&out); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if len(out.Items) != 0 {
		t.Errorf("items = %d, want 0 for an unknown repo", len(out.Items))
	}
}
