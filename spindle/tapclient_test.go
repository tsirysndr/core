package spindle

import (
	"context"
	"encoding/json"
	"log/slog"
	"strings"
	"tangled.org/core/jetstream"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/idresolver"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"

	"tangled.org/core/tapc"
)

func TestProcessRepo_MembershipCheck(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	ccfg := eventconsumer.NewConsumerConfig()
	ccfg.Logger = slog.Default()
	ks := eventconsumer.NewConsumer(*ccfg)

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}
	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		ks:      ks,
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	ownerDid := syntax.DID("did:plc:memberowner")
	nonMemberDid := syntax.DID("did:plc:nonmemberowner")
	repoDid := "did:plc:testrepo123"

	err := e.AddSpindle(rbac.ThisServer)
	if err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}
	err = e.AddSpindleMember(rbac.ThisServer, ownerDid.String())
	if err != nil {
		t.Fatalf("AddSpindleMember: %v", err)
	}

	recNonMember := tangled.Repo{
		Knot:      "knot.test",
		RepoDid:   &repoDid,
		Spindle:   &cfg.Server.Hostname,
		CreatedAt: time.Now().Format(time.RFC3339),
	}
	recNonMemberJson, _ := json.Marshal(recNonMember)

	err = tap.processRepo(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        nonMemberDid,
		Rkey:       "test-repo-rkey",
		Collection: syntax.NSID(tangled.RepoNSID),
		Action:     tapc.RecordCreateAction,
		Record:     recNonMemberJson,
	})
	if err != nil {
		t.Fatalf("processRepo returned error for non-member: %v", err)
	}

	_, err = d.GetRepoByOwnerRkey(nonMemberDid, "test-repo-rkey")
	if err == nil {
		t.Fatal("repo for non-member was registered in DB, expected rejection")
	}

	recMember := tangled.Repo{
		Knot:      "knot.test",
		RepoDid:   &repoDid,
		Spindle:   &cfg.Server.Hostname,
		CreatedAt: time.Now().Format(time.RFC3339),
	}
	recMemberJson, _ := json.Marshal(recMember)

	err = tap.processRepo(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        ownerDid,
		Rkey:       "test-repo-rkey",
		Collection: syntax.NSID(tangled.RepoNSID),
		Action:     tapc.RecordCreateAction,
		Record:     recMemberJson,
	})
	if err == nil {
		t.Fatal("expected git clone error for valid member, but got nil")
	}

	if !strings.Contains(err.Error(), "setting up sparse-clone git repo") {
		t.Fatalf("expected sparse-clone error, got: %v", err)
	}
}

func TestProcessPull_PushAllowedCheck(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}
	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		res:     idresolver.DefaultResolver("https://plc.test"),
		jc:      jc,
		rootCtx: context.Background(),
	}

	repoOwnerDid := syntax.DID("did:plc:repoowner")
	nonPusherDid := syntax.DID("did:plc:nonpusher")
	pusherDid := syntax.DID("did:plc:pusher")
	repoDid := syntax.DID("did:plc:testrepo123")

	err := d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     repoOwnerDid,
		Rkey:      "test-repo-rkey",
		RepoDid:   repoDid,
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("AddRepo: %v", err)
	}

	err = e.AddRepo(repoOwnerDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddRepo permissions: %v", err)
	}
	err = e.AddCollaborator(pusherDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddCollaborator: %v", err)
	}

	pullRecord := tangled.RepoPull{
		Target: &tangled.RepoPull_Target{
			Branch: "main",
			Repo:   repoDid.String(),
		},
		Source: &tangled.RepoPull_Source{
			Branch: "feature",
			Repo:   nil, // branch-based PR (source repo is nil)
		},
	}
	pullRecordJson, _ := json.Marshal(pullRecord)

	err = s.processPull(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        nonPusherDid,
		Rkey:       "pull-rkey-1",
		Collection: syntax.NSID(tangled.RepoPullNSID),
		Action:     tapc.RecordCreateAction,
		Record:     pullRecordJson,
	})
	if err != nil {
		t.Fatalf("processPull returned error for non-pusher: %v", err)
	}

	// fetch fails because plc/pds are not real
	err = s.processPull(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        pusherDid,
		Rkey:       "pull-rkey-2",
		Collection: syntax.NSID(tangled.RepoPullNSID),
		Action:     tapc.RecordCreateAction,
		Record:     pullRecordJson,
	})
	if err == nil {
		t.Fatal("expected error from fetchLatestSubmission for valid pusher, but got nil")
	}

	if !strings.Contains(err.Error(), "checking push access") && !strings.Contains(err.Error(), "resolve PR owner") && !strings.Contains(err.Error(), "invalid memory address") {
		t.Fatalf("expected failed identity resolution or connection error, got: %v", err)
	}
}

func TestProcessRepo_HijackRepoDidCheck(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	ccfg := eventconsumer.NewConsumerConfig()
	ccfg.Logger = slog.Default()
	ks := eventconsumer.NewConsumer(*ccfg)

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}
	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		ks:      ks,
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	aliceDid := syntax.DID("did:plc:alice")
	bobDid := syntax.DID("did:plc:bob")
	repoDid := "did:plc:sharedrepo"

	err := e.AddSpindle(rbac.ThisServer)
	if err != nil {
		t.Fatalf("AddSpindle: %v", err)
	}
	err = e.AddSpindleMember(rbac.ThisServer, aliceDid.String())
	if err != nil {
		t.Fatalf("AddSpindleMember alice: %v", err)
	}
	err = e.AddSpindleMember(rbac.ThisServer, bobDid.String())
	if err != nil {
		t.Fatalf("AddSpindleMember bob: %v", err)
	}

	err = d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     aliceDid,
		Rkey:      "alice-repo",
		RepoDid:   syntax.DID(repoDid),
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("d.AddRepo: %v", err)
	}

	// bob tries to register alice's repo did, must reject the hijack
	recBob := tangled.Repo{
		Knot:      "knot.test",
		RepoDid:   &repoDid,
		Spindle:   &cfg.Server.Hostname,
		CreatedAt: time.Now().Format(time.RFC3339),
	}
	recBobJson, _ := json.Marshal(recBob)

	err = tap.processRepo(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        bobDid,
		Rkey:       "bob-repo",
		Collection: syntax.NSID(tangled.RepoNSID),
		Action:     tapc.RecordCreateAction,
		Record:     recBobJson,
	})
	if err != nil {
		t.Fatalf("processRepo returned error on duplicate repoDid hijack attempt: %v", err)
	}

	_, err = d.GetRepoByOwnerRkey(bobDid, "bob-repo")
	if err == nil {
		t.Fatal("bob successfully hijacked alice's repoDid in DB, expected rejection")
	}
}

func TestProcessCollaborator_RBAC(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	ownerDid := syntax.DID("did:plc:repoowner")
	otherDid := syntax.DID("did:plc:otheractor")
	subjectDid := syntax.DID("did:plc:collabsubject")
	repoDid := syntax.DID("did:plc:testrepo123")

	h, err := syntax.ParseHandle("collabsubject.test")
	if err != nil {
		t.Fatalf("syntax.ParseHandle: %v", err)
	}
	mockIdent := &identity.Identity{
		DID:    subjectDid,
		Handle: h,
	}
	resolver := idresolver.NewMockResolver(idresolver.MockDirectory{Ident: mockIdent})

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}
	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		res:     resolver,
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	err = d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     ownerDid,
		Rkey:      "test-repo-rkey",
		RepoDid:   repoDid,
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("AddRepo: %v", err)
	}

	collabRecord := tangled.RepoCollaborator{
		Subject: subjectDid.String(),
		Repo:    repoDid.String(),
	}
	collabRecordJson, _ := json.Marshal(collabRecord)

	err = tap.processCollaborator(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        otherDid,
		Rkey:       "collab-rkey-1",
		Collection: syntax.NSID(tangled.RepoCollaboratorNSID),
		Action:     tapc.RecordCreateAction,
		Record:     collabRecordJson,
	})
	if err != nil {
		t.Fatalf("processCollaborator returned error: %v", err)
	}

	_, err = d.GetRepoCollaborator(otherDid, "collab-rkey-1")
	if err == nil {
		t.Fatal("collaborator from non-owner was registered in DB")
	}

	err = tap.processCollaborator(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        ownerDid,
		Rkey:       "collab-rkey-2",
		Collection: syntax.NSID(tangled.RepoCollaboratorNSID),
		Action:     tapc.RecordCreateAction,
		Record:     collabRecordJson,
	})
	if err != nil {
		t.Fatalf("processCollaborator returned error: %v", err)
	}
	_, err = d.GetRepoCollaborator(ownerDid, "collab-rkey-2")
	if err == nil {
		t.Fatal("collaborator registered despite missing Casbin invite permission")
	}

	err = e.AddRepo(ownerDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddRepo permissions: %v", err)
	}

	err = tap.processCollaborator(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        ownerDid,
		Rkey:       "collab-rkey-3",
		Collection: syntax.NSID(tangled.RepoCollaboratorNSID),
		Action:     tapc.RecordCreateAction,
		Record:     collabRecordJson,
	})
	if err != nil {
		t.Fatalf("processCollaborator failed for authorized owner: %v", err)
	}

	c, err := d.GetRepoCollaborator(ownerDid, "collab-rkey-3")
	if err != nil {
		t.Fatalf("GetRepoCollaborator error: %v", err)
	}
	if c.Subject != subjectDid || c.RepoDid != repoDid {
		t.Fatalf("unexpected collaborator: %+v", c)
	}

	ok, err := e.IsRepoCollaborator(subjectDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || !ok {
		t.Fatalf("Casbin policy for collaborator missing or err: %v", err)
	}

	err = tap.processCollaborator(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        ownerDid,
		Rkey:       "collab-rkey-3",
		Collection: syntax.NSID(tangled.RepoCollaboratorNSID),
		Action:     tapc.RecordDeleteAction,
	})
	if err != nil {
		t.Fatalf("delete collaborator process returned error: %v", err)
	}

	_, err = d.GetRepoCollaborator(ownerDid, "collab-rkey-3")
	if err == nil {
		t.Fatal("collaborator DB row remained after deletion")
	}

	ok, err = e.IsRepoCollaborator(subjectDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || ok {
		t.Fatal("Casbin policy for collaborator remained after deletion")
	}
}

func TestTeardownRepo_RBAC(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}
	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	ownerDid := syntax.DID("did:plc:repoowner")
	repoDid := syntax.DID("did:plc:testrepo123")
	collabDid := syntax.DID("did:plc:collab")

	err := d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     ownerDid,
		Rkey:      "test-repo-rkey",
		RepoDid:   repoDid,
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("AddRepo DB: %v", err)
	}

	err = e.AddRepo(ownerDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddRepo policy: %v", err)
	}

	err = d.AddRepoCollaborator(db.RepoCollaborator{
		OwnerDid: ownerDid,
		Rkey:     "collab-rkey",
		Subject:  collabDid,
		RepoDid:  repoDid,
	})
	if err != nil {
		t.Fatalf("AddCollaborator DB: %v", err)
	}

	err = e.AddCollaborator(collabDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddCollaborator policy: %v", err)
	}

	err = tap.processRepo(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        ownerDid,
		Rkey:       "test-repo-rkey",
		Collection: syntax.NSID(tangled.RepoNSID),
		Action:     tapc.RecordDeleteAction,
	})
	if err != nil {
		t.Fatalf("processRepo delete returned error: %v", err)
	}

	_, err = d.GetRepoByOwnerRkey(ownerDid, "test-repo-rkey")
	if err == nil {
		t.Fatal("repo remained in DB after delete")
	}

	collabs, err := d.ListCollaboratorsByRepoDid(repoDid)
	if err != nil {
		t.Fatalf("ListCollaboratorsByRepoDid: %v", err)
	}
	if len(collabs) > 0 {
		t.Fatal("collaborators remained in DB after delete")
	}

	ok, err := e.IsRepoOwner(ownerDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || ok {
		t.Fatal("repo owner policy remained in Casbin after delete")
	}

	ok, err = e.IsRepoCollaborator(collabDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || ok {
		t.Fatal("collaborator policy remained in Casbin after delete")
	}
}

func TestProcessRepo_ForgeDeleteRejection(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}

	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	aliceDid := syntax.DID("did:plc:alice")
	bobDid := syntax.DID("did:plc:bob")
	repoDid := syntax.DID("did:plc:sharedrepo")

	err := d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     aliceDid,
		Rkey:      "test-repo-rkey",
		RepoDid:   repoDid,
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("AddRepo DB: %v", err)
	}

	err = e.AddRepo(aliceDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddRepo policy: %v", err)
	}

	// bob tries to delete alice's repo, must reject forged delete
	err = tap.processRepo(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        bobDid,
		Rkey:       "test-repo-rkey",
		Collection: syntax.NSID(tangled.RepoNSID),
		Action:     tapc.RecordDeleteAction,
	})
	if err != nil {
		t.Fatalf("processRepo returned error on delete: %v", err)
	}

	_, err = d.GetRepoByOwnerRkey(aliceDid, "test-repo-rkey")
	if err != nil {
		t.Fatalf("Alice's repo was deleted or error: %v", err)
	}

	ok, err := e.IsRepoOwner(aliceDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || !ok {
		t.Fatal("Alice's owner policy was removed from Casbin by forged delete")
	}
}

func TestProcessCollaborator_ForgeDeleteRejection(t *testing.T) {
	d, e := newTestSpindleDB(t)

	cfg := &config.Config{}
	cfg.Server.Hostname = "spindle.test"

	jc, jcerr := jetstream.NewJetstreamClient("", "", nil, nil, slog.Default(), nil, false, false)
	if jcerr != nil {
		t.Fatalf("NewJetstreamClient: %v", jcerr)
	}

	s := &Spindle{
		db:      d,
		e:       e,
		l:       slog.Default(),
		cfg:     cfg,
		res:     idresolver.DefaultResolver("https://plc.test"),
		jc:      jc,
		rootCtx: context.Background(),
	}

	tap := &Tap{
		spindle: s,
		logger:  slog.Default(),
	}

	ownerDid := syntax.DID("did:plc:repoowner")
	bobDid := syntax.DID("did:plc:bob")
	collabDid := syntax.DID("did:plc:collab")
	repoDid := syntax.DID("did:plc:testrepo123")

	err := d.AddRepo(db.Repo{
		Knot:      "knot.test",
		Owner:     ownerDid,
		Rkey:      "test-repo-rkey",
		RepoDid:   repoDid,
		CreatedAt: time.Now().Format(time.RFC3339),
	})
	if err != nil {
		t.Fatalf("AddRepo: %v", err)
	}

	err = e.AddRepo(ownerDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddRepo permissions: %v", err)
	}

	err = d.AddRepoCollaborator(db.RepoCollaborator{
		OwnerDid: ownerDid,
		Rkey:     "collab-rkey",
		Subject:  collabDid,
		RepoDid:  repoDid,
	})
	if err != nil {
		t.Fatalf("AddRepoCollaborator: %v", err)
	}

	err = e.AddCollaborator(collabDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil {
		t.Fatalf("AddCollaborator policy: %v", err)
	}

	// bob tries to delete alice's collaborator, must reject forged delete
	err = tap.processCollaborator(context.Background(), &tapc.RecordEventData{
		Live:       true,
		Did:        bobDid,
		Rkey:       "collab-rkey",
		Collection: syntax.NSID(tangled.RepoCollaboratorNSID),
		Action:     tapc.RecordDeleteAction,
	})
	if err != nil {
		t.Fatalf("processCollaborator delete returned error: %v", err)
	}

	_, err = d.GetRepoCollaborator(ownerDid, "collab-rkey")
	if err != nil {
		t.Fatalf("collaborator was deleted from DB: %v", err)
	}

	ok, err := e.IsRepoCollaborator(collabDid.String(), rbac.ThisServer, repoDid.String())
	if err != nil || !ok {
		t.Fatal("collaborator policy was removed from Casbin by forged delete")
	}
}
