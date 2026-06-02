package xrpc

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"tangled.org/core/api/tangled"
	"tangled.org/core/eventstream"
	"tangled.org/core/knotserver/db"
)

func eventsOfType(t *testing.T, x *Xrpc, nsid string) []eventstream.Event {
	t.Helper()
	all, err := x.Db.GetEvents(0, 1000)
	if err != nil {
		t.Fatalf("GetEvents: %v", err)
	}
	var out []eventstream.Event
	for _, e := range all {
		if e.Nsid == nsid {
			out = append(out, e)
		}
	}
	return out
}

func decodeMemberUpdate(t *testing.T, e eventstream.Event) db.KnotMemberUpdate {
	t.Helper()
	var m db.KnotMemberUpdate
	if err := json.Unmarshal(e.EventJson, &m); err != nil {
		t.Fatalf("decode memberUpdate: %v", err)
	}
	return m
}

func decodeCollaboratorUpdate(t *testing.T, e eventstream.Event) db.RepoCollaboratorUpdate {
	t.Helper()
	var c db.RepoCollaboratorUpdate
	if err := json.Unmarshal(e.EventJson, &c); err != nil {
		t.Fatalf("decode collaboratorUpdate: %v", err)
	}
	return c
}

func TestAddMember_EmitsAddEvent(t *testing.T) {
	x, _ := newACLXrpc(t)
	rec := httptest.NewRecorder()
	x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, body=%s", rec.Code, rec.Body.String())
	}

	evs := eventsOfType(t, x, db.KnotMemberUpdateNSID)
	if len(evs) != 1 {
		t.Fatalf("memberUpdate events = %d, want 1", len(evs))
	}
	m := decodeMemberUpdate(t, evs[0])
	if m.Op != db.AclOpAdd || m.Subject != aclSubject {
		t.Errorf("event = %+v, want op=add subject=%s", m, aclSubject)
	}
}

func TestRemoveMember_EmitsRemoveEvent(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedMembers(t, x, aclSubject)

	rec := httptest.NewRecorder()
	x.RemoveMember(rec, aclRequest(t, aclOwner, tangled.KnotRemoveMember_Input{Subject: aclSubject}))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, body=%s", rec.Code, rec.Body.String())
	}

	evs := eventsOfType(t, x, db.KnotMemberUpdateNSID)
	if len(evs) != 2 {
		t.Fatalf("memberUpdate events = %d, want 2 (add then remove)", len(evs))
	}
	last := decodeMemberUpdate(t, evs[len(evs)-1])
	if last.Op != db.AclOpRemove || last.Subject != aclSubject {
		t.Errorf("last event = %+v, want op=remove subject=%s", last, aclSubject)
	}
}

func TestAddMember_NoOpDoesNotEmit(t *testing.T) {
	x, _ := newACLXrpc(t)
	for range 2 {
		rec := httptest.NewRecorder()
		x.AddMember(rec, aclRequest(t, aclOwner, tangled.KnotAddMember_Input{Subject: aclSubject}))
		if rec.Code != http.StatusOK {
			t.Fatalf("status = %d, body=%s", rec.Code, rec.Body.String())
		}
	}

	if evs := eventsOfType(t, x, db.KnotMemberUpdateNSID); len(evs) != 1 {
		t.Errorf("memberUpdate events = %d, want 1; a duplicate grant is a no-op and must not emit", len(evs))
	}
}

func TestCollaborator_EmitsAddThenRemove(t *testing.T) {
	x, _ := newACLXrpc(t)
	seedRepo(t, x)

	addRec := httptest.NewRecorder()
	x.AddCollaborator(addRec, aclRequest(t, aclOwner, tangled.RepoAddCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if addRec.Code != http.StatusOK {
		t.Fatalf("add status = %d, body=%s", addRec.Code, addRec.Body.String())
	}

	rmRec := httptest.NewRecorder()
	x.RemoveCollaborator(rmRec, aclRequest(t, aclOwner, tangled.RepoRemoveCollaborator_Input{Repo: aclRepoDid, Subject: aclSubject}))
	if rmRec.Code != http.StatusOK {
		t.Fatalf("remove status = %d, body=%s", rmRec.Code, rmRec.Body.String())
	}

	evs := eventsOfType(t, x, db.RepoCollaboratorUpdateNSID)
	if len(evs) != 2 {
		t.Fatalf("collaboratorUpdate events = %d, want 2 (add then remove)", len(evs))
	}

	add := decodeCollaboratorUpdate(t, evs[0])
	if add.Op != db.AclOpAdd || add.Subject != aclSubject || add.Repo != aclRepoDid {
		t.Errorf("add event = %+v, want op=add subject=%s repo=%s", add, aclSubject, aclRepoDid)
	}
	rm := decodeCollaboratorUpdate(t, evs[1])
	if rm.Op != db.AclOpRemove || rm.Subject != aclSubject || rm.Repo != aclRepoDid {
		t.Errorf("remove event = %+v, want op=remove subject=%s repo=%s", rm, aclSubject, aclRepoDid)
	}
}
