package state

import (
	"context"
	"encoding/json"
	"errors"
	"path/filepath"
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/models"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventstream"
	knotdb "tangled.org/core/knotserver/db"
)

const (
	aclTestHost    = "knot.nel.pet"
	aclTestRepoDid = "did:plc:limpet"
	aclTestOwner   = "did:plc:akshay"
	aclTestSubject = "did:plc:boltless"
)

type memberCall struct {
	host    string
	subject string
}

type collabCall struct {
	repoDid string
	subject string
}

type recordingAcl struct {
	memberAdd      []memberCall
	memberRemove   []memberCall
	collabAdd      []collabCall
	collabRemove   []collabCall
	membersInvalid []string
	collabsInvalid []collabCall
}

func (r *recordingAcl) AddKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error {
	r.memberAdd = append(r.memberAdd, memberCall{host, subject.String()})
	return nil
}

func (r *recordingAcl) RemoveKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error {
	r.memberRemove = append(r.memberRemove, memberCall{host, subject.String()})
	return nil
}

func (r *recordingAcl) AddCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error {
	r.collabAdd = append(r.collabAdd, collabCall{repoDid.String(), subject.String()})
	return nil
}

func (r *recordingAcl) RemoveCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error {
	r.collabRemove = append(r.collabRemove, collabCall{repoDid.String(), subject.String()})
	return nil
}

func (r *recordingAcl) InvalidateMembers(host string) {
	r.membersInvalid = append(r.membersInvalid, host)
}

func (r *recordingAcl) InvalidateCollaborators(host, repoDid string) {
	r.collabsInvalid = append(r.collabsInvalid, collabCall{repoDid, host})
}

type flakyAcl struct {
	failsLeft      int
	calls          int
	membersInvalid int
	collabsInvalid int
}

func (a *flakyAcl) try() error {
	a.calls++
	if a.failsLeft > 0 {
		a.failsLeft--
		return errors.New("transient store error")
	}
	return nil
}

func (a *flakyAcl) AddKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error {
	return a.try()
}
func (a *flakyAcl) RemoveKnotMember(host string, subject syntax.DID, cursor knotacl.Cursor) error {
	return a.try()
}
func (a *flakyAcl) AddCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error {
	return a.try()
}
func (a *flakyAcl) RemoveCollaborator(repoDid, subject syntax.DID, cursor knotacl.Cursor) error {
	return a.try()
}
func (a *flakyAcl) InvalidateMembers(host string)             { a.membersInvalid++ }
func (a *flakyAcl) InvalidateCollaborators(host, repo string) { a.collabsInvalid++ }

func aclTestDB(t *testing.T) *db.DB {
	t.Helper()
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "appview.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	return d
}

func seedAclRepo(t *testing.T, d *db.DB) {
	t.Helper()
	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("begin: %v", err)
	}
	if err := db.AddRepo(tx, &models.Repo{
		Did:     aclTestOwner,
		Knot:    aclTestHost,
		RepoDid: aclTestRepoDid,
		Name:    "anemone",
	}); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
}

func memberEvent(t *testing.T, op knotdb.AclOp, subject string) eventstream.Event {
	t.Helper()
	payload, err := json.Marshal(knotdb.KnotMemberUpdate{Op: op, Subject: subject})
	if err != nil {
		t.Fatalf("marshal memberUpdate: %v", err)
	}
	return eventstream.Event{Rkey: "evt", Nsid: knotdb.KnotMemberUpdateNSID, EventJson: payload}
}

func collabEvent(t *testing.T, op knotdb.AclOp, subject, repoDid string) eventstream.Event {
	t.Helper()
	payload, err := json.Marshal(knotdb.RepoCollaboratorUpdate{Op: op, Subject: subject, Repo: repoDid})
	if err != nil {
		t.Fatalf("marshal collaboratorUpdate: %v", err)
	}
	return eventstream.Event{Rkey: "evt", Nsid: knotdb.RepoCollaboratorUpdateNSID, EventJson: payload}
}

func TestIngestKnotMemberUpdate_DispatchesAddThenRemove(t *testing.T) {
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpAdd, aclTestSubject)); err != nil {
		t.Fatalf("add: %v", err)
	}
	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpRemove, aclTestSubject)); err != nil {
		t.Fatalf("remove: %v", err)
	}

	if len(acl.memberAdd) != 1 || acl.memberAdd[0] != (memberCall{aclTestHost, aclTestSubject}) {
		t.Errorf("memberAdd = %v, want one add scoped to the source host", acl.memberAdd)
	}
	if len(acl.memberRemove) != 1 || acl.memberRemove[0] != (memberCall{aclTestHost, aclTestSubject}) {
		t.Errorf("memberRemove = %v, want one remove scoped to the source host", acl.memberRemove)
	}
}

func TestIngestKnotMemberUpdate_UnknownOpErrors(t *testing.T) {
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}
	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOp("bogus"), aclTestSubject)); err == nil {
		t.Fatal("an unknown op must be rejected")
	}
	if len(acl.memberAdd)+len(acl.memberRemove) != 0 {
		t.Errorf("an unknown op must not reach the roster: %+v", acl)
	}
}

func TestIngestKnotMemberUpdate_BadSubjectErrors(t *testing.T) {
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}
	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpAdd, "not-a-did")); err == nil {
		t.Fatal("a malformed subject DID must be rejected")
	}
	if len(acl.memberAdd) != 0 {
		t.Errorf("a malformed subject must not reach the roster: %v", acl.memberAdd)
	}
}

func TestIngestCollaboratorUpdate_DispatchesAddThenRemove(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	seedAclRepo(t, d)
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, aclTestSubject, aclTestRepoDid)); err != nil {
		t.Fatalf("add: %v", err)
	}
	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpRemove, aclTestSubject, aclTestRepoDid)); err != nil {
		t.Fatalf("remove: %v", err)
	}

	if len(acl.collabAdd) != 1 || acl.collabAdd[0] != (collabCall{aclTestRepoDid, aclTestSubject}) {
		t.Errorf("collabAdd = %v, want one add for the repo", acl.collabAdd)
	}
	if len(acl.collabRemove) != 1 || acl.collabRemove[0] != (collabCall{aclTestRepoDid, aclTestSubject}) {
		t.Errorf("collabRemove = %v, want one remove for the repo", acl.collabRemove)
	}
}

func TestIngestCollaboratorUpdate_UnindexedRepoSkips(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, aclTestSubject, aclTestRepoDid)); err != nil {
		t.Fatalf("add for unindexed repo must not error, got: %v", err)
	}
	if len(acl.collabAdd) != 0 {
		t.Errorf("an add for an unindexed repo must not reach the roster: %v", acl.collabAdd)
	}
}

func TestIngestCollaboratorUpdate_ForeignKnotDropped(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	seedAclRepo(t, d)
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: "barnacle.nel.pet"}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, aclTestSubject, aclTestRepoDid)); err != nil {
		t.Fatalf("a foreign-knot collaboratorUpdate must be dropped, not error: %v", err)
	}
	if len(acl.collabAdd) != 0 {
		t.Errorf("a knot that does not host the repo must not mutate its collaborators: %v", acl.collabAdd)
	}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpRemove, aclTestSubject, aclTestRepoDid)); err != nil {
		t.Fatalf("a foreign-knot remove must be dropped, not error: %v", err)
	}
	if len(acl.collabRemove) != 0 {
		t.Errorf("a knot that does not host the repo must not remove its collaborators: %v", acl.collabRemove)
	}
}

func TestIngestCollaboratorUpdate_BadDidErrors(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}
	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, "not-a-did", aclTestRepoDid)); err == nil {
		t.Fatal("a malformed subject DID must be rejected")
	}
	if len(acl.collabAdd) != 0 {
		t.Errorf("a malformed subject must not reach the roster: %v", acl.collabAdd)
	}
}

func TestIngestKnotMemberUpdate_RetriesTransientThenSucceeds(t *testing.T) {
	acl := &flakyAcl{failsLeft: aclIngestAttempts - 1}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpAdd, aclTestSubject)); err != nil {
		t.Fatalf("a transient store error within the retry budget must recover, got: %v", err)
	}
	if acl.calls != aclIngestAttempts {
		t.Errorf("calls = %d, want %d; the write must retry until it lands", acl.calls, aclIngestAttempts)
	}
}

func TestIngestKnotMemberUpdate_GivesUpAfterAttempts(t *testing.T) {
	acl := &flakyAcl{failsLeft: aclIngestAttempts + 5}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpAdd, aclTestSubject)); err == nil {
		t.Fatal("a persistent store error must surface so the failure is logged")
	}
	if acl.calls != aclIngestAttempts {
		t.Errorf("calls = %d, want %d; the retry must be bounded", acl.calls, aclIngestAttempts)
	}
	if acl.membersInvalid != 1 {
		t.Errorf("membersInvalid = %d, want 1; a dropped delta must invalidate the scope so the next read reconciles instead of waiting out the TTL", acl.membersInvalid)
	}
}

func TestIngestCollaboratorUpdate_InvalidatesScopeOnGiveUp(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	seedAclRepo(t, d)
	acl := &flakyAcl{failsLeft: aclIngestAttempts + 5}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, aclTestSubject, aclTestRepoDid)); err == nil {
		t.Fatal("a persistent store error must surface so the failure is logged")
	}
	if acl.collabsInvalid != 1 {
		t.Errorf("collabsInvalid = %d, want 1; a dropped delta must invalidate the scope", acl.collabsInvalid)
	}
}

func TestIngestKnotMemberUpdate_NoInvalidateOnSuccess(t *testing.T) {
	acl := &flakyAcl{failsLeft: aclIngestAttempts - 1}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestKnotMemberUpdate(acl, source, memberEvent(t, knotdb.AclOpAdd, aclTestSubject)); err != nil {
		t.Fatalf("a recoverable delta must not error: %v", err)
	}
	if acl.membersInvalid != 0 {
		t.Errorf("membersInvalid = %d, want 0; a delta that lands must not force a reconcile", acl.membersInvalid)
	}
}

func TestIngestCollaboratorUpdate_StoreErrorPropagates(t *testing.T) {
	ctx := context.Background()
	d := aclTestDB(t)
	if err := d.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	acl := &recordingAcl{}
	source := ec.Source{Kind: ec.KindKnot, Host: aclTestHost}

	if err := ingestCollaboratorUpdate(ctx, d, acl, source, collabEvent(t, knotdb.AclOpAdd, aclTestSubject, aclTestRepoDid)); err == nil {
		t.Fatal("a store error on the repo lookup must surface, not be swallowed as an unindexed-repo skip")
	}
	if len(acl.collabAdd) != 0 {
		t.Errorf("a failed repo lookup must not reach the roster: %v", acl.collabAdd)
	}
}
