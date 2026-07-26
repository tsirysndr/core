package xrpc

import (
	"bytes"
	"context"
	"encoding/json"
	"github.com/bluesky-social/indigo/atproto/identity"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/idresolver"
	"tangled.org/core/rbac"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

type mockTrigger struct {
	triggered bool
}

func (m *mockTrigger) TriggerManual(ctx context.Context, repoDid syntax.DID, sha, ref string, workflows []string, sourceRepo syntax.DID, pull PullContext, inputs []*tangled.Pipeline_Pair) (syntax.ATURI, error) {
	m.triggered = true
	return syntax.ParseATURI("at://did:plc:repoowner/sh.tangled.ci.pipeline/testrkey")
}

func newTestXrpcDB(t *testing.T) (*db.DB, *rbac.Enforcer) {
	t.Helper()
	p := filepath.Join(t.TempDir(), "spindle_xrpc.db")
	d, err := db.Make(context.Background(), p)
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	e, err := rbac.NewEnforcer(p)
	if err != nil {
		t.Fatalf("rbac.NewEnforcer: %v", err)
	}
	e.E.EnableAutoSave(true)
	return d, e
}

func TestTriggerPipeline_RBAC(t *testing.T) {
	d, e := newTestXrpcDB(t)

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

	trigger := &mockTrigger{}
	x := &Xrpc{
		Logger:   slog.Default(),
		Db:       d,
		Enforcer: e,
		Config:   &config.Config{},
		Trigger:  trigger,
	}

	sendReq := func(actor syntax.DID, input tangled.CiTriggerPipeline_Input) (*httptest.ResponseRecorder, int) {
		body, _ := json.Marshal(input)
		req := httptest.NewRequest(http.MethodPost, "/com.atproto.repo.createRecord", bytes.NewReader(body))
		ctx := context.WithValue(req.Context(), ActorDid, actor)
		req = req.WithContext(ctx)

		w := httptest.NewRecorder()
		x.TriggerPipeline(w, req)
		return w, w.Code
	}

	sha := "0123456789abcdef0123456789abcdef01234567"
	ref := "refs/heads/main"

	input := tangled.CiTriggerPipeline_Input{
		Repo: repoDid.String(),
		Trigger: &tangled.CiTriggerPipeline_Input_Trigger{
			CiTrigger_Manual: &tangled.CiTrigger_Manual{
				Sha: sha,
				Ref: &ref,
			},
		},
	}

	w, code := sendReq(pusherDid, input)
	if code != http.StatusOK {
		t.Fatalf("expected 200 for pusher, got %d (body: %s)", code, w.Body.String())
	}
	if !trigger.triggered {
		t.Fatal("expected pipeline trigger to be called")
	}

	trigger.triggered = false

	w, code = sendReq(nonPusherDid, input)
	if code != http.StatusBadRequest {
		t.Fatalf("expected 400 for non-pusher, got %d", code)
	}
	if !strings.Contains(w.Body.String(), "AccessControl") {
		t.Fatalf("expected AccessControl, got: %s", w.Body.String())
	}
	if trigger.triggered {
		t.Fatal("expected pipeline trigger not to be called for non-pusher")
	}

	badInput := input
	badInput.Repo = "did:plc:unknownrepo"
	w, code = sendReq(pusherDid, badInput)
	if code != http.StatusBadRequest {
		t.Fatalf("expected 400 for unknown repo, got %d", code)
	}
	if !strings.Contains(w.Body.String(), "RepoNotFound") {
		t.Fatalf("expected RepoNotFound, got: %s", w.Body.String())
	}
}

func TestCancelPipeline_RBAC(t *testing.T) {
	d, e := newTestXrpcDB(t)

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

	pipelineTid := "3mrkp6iz6os2o"
	repoDidStr := repoDid.String()
	tpl := tangled.Pipeline{
		TriggerMetadata: &tangled.Pipeline_TriggerMetadata{
			Kind: "manual",
			Repo: &tangled.Pipeline_TriggerRepo{
				RepoDid: &repoDidStr,
				Knot:    "knot.test",
				Did:     repoOwnerDid.String(),
			},
		},
		Workflows: []*tangled.Pipeline_Workflow{
			{Name: "test-workflow"},
		},
	}
	err = d.CreatePipelineEvent(pipelineTid, tpl, nil)
	if err != nil {
		t.Fatalf("CreatePipelineEvent: %v", err)
	}

	_, err = d.Exec(`UPDATE pipelines SET repo_did = ? WHERE id = ?`, repoDid.String(), pipelineTid)
	if err != nil {
		t.Fatalf("Update pipeline repo association: %v", err)
	}

	x := &Xrpc{
		Logger:   slog.Default(),
		Db:       d,
		Enforcer: e,
		Config:   &config.Config{},
		Engines:  make(map[string]models.Engine),
	}

	sendReq := func(actor syntax.DID, input tangled.CiCancelPipeline_Input) (*httptest.ResponseRecorder, int) {
		body, _ := json.Marshal(input)
		req := httptest.NewRequest(http.MethodPost, "/com.atproto.repo.createRecord", bytes.NewReader(body))
		ctx := context.WithValue(req.Context(), ActorDid, actor)
		req = req.WithContext(ctx)

		w := httptest.NewRecorder()
		x.CancelPipeline(w, req)
		return w, w.Code
	}

	input := tangled.CiCancelPipeline_Input{
		Repo:     repoDid.String(),
		Pipeline: pipelineTid,
	}

	w, code := sendReq(pusherDid, input)
	if code != http.StatusOK {
		t.Fatalf("expected 200 for pusher, got %d (body: %s)", code, w.Body.String())
	}

	w, code = sendReq(nonPusherDid, input)
	if code != http.StatusBadRequest {
		t.Fatalf("expected 400 for non-pusher, got %d", code)
	}
	if !strings.Contains(w.Body.String(), "AccessControl") {
		t.Fatalf("expected AccessControl, got: %s", w.Body.String())
	}
}

type mockDirectory struct {
	ident *identity.Identity
}

func (m *mockDirectory) LookupDID(ctx context.Context, did syntax.DID) (*identity.Identity, error) {
	return m.ident, nil
}

func (m *mockDirectory) LookupHandle(ctx context.Context, handle syntax.Handle) (*identity.Identity, error) {
	return m.ident, nil
}

func (m *mockDirectory) Lookup(ctx context.Context, id syntax.AtIdentifier) (*identity.Identity, error) {
	return m.ident, nil
}

func (m *mockDirectory) Purge(ctx context.Context, id syntax.AtIdentifier) error {
	return nil
}

func TestSecrets_RBAC(t *testing.T) {
	d, e := newTestXrpcDB(t)

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

	vault, err := secrets.NewSQLiteManager(":memory:")
	if err != nil {
		t.Fatalf("secrets.NewSQLiteManager: %v", err)
	}

	var ts *httptest.Server
	ts = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasPrefix(r.URL.Path, "/xrpc/com.atproto.repo.getRecord") {
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{
				"uri": "at://did:plc:repoowner/sh.tangled.repo/test-repo-rkey",
				"cid": "bafybeigdyrzt5s2nuxwos7552",
				"value": {
					"$type": "sh.tangled.repo",
					"knot": "knot.test",
					"repoDid": "did:plc:testrepo123",
					"spindle": "spindle.test",
					"createdAt": "2026-07-26T12:00:00Z"
				}
			}`))
			return
		}
		w.WriteHeader(http.StatusNotFound)
	}))
	defer ts.Close()

	h, err := syntax.ParseHandle("repoowner.test")
	if err != nil {
		t.Fatalf("syntax.ParseHandle: %v", err)
	}

	mockIdent := &identity.Identity{
		DID:    repoOwnerDid,
		Handle: h,
		Services: map[string]identity.ServiceEndpoint{
			"atproto_pds": {
				Type: "AtprotoPersonalDataServer",
				URL:  ts.URL,
			},
		},
	}

	resolver := idresolver.NewMockResolver(&mockDirectory{ident: mockIdent})

	x := &Xrpc{
		Logger:   slog.Default(),
		Db:       d,
		Enforcer: e,
		Config:   &config.Config{},
		Resolver: resolver,
		Vault:    vault,
	}

	addInput := tangled.RepoAddSecret_Input{
		Repo:  "at://did:plc:repoowner/sh.tangled.repo/test-repo-rkey",
		Key:   "MY_SECRET",
		Value: "supersecret",
	}

	sendAdd := func(actor syntax.DID, input tangled.RepoAddSecret_Input) (*httptest.ResponseRecorder, int) {
		body, _ := json.Marshal(input)
		req := httptest.NewRequest(http.MethodPost, "/"+tangled.RepoAddSecretNSID, bytes.NewReader(body))
		ctx := context.WithValue(req.Context(), ActorDid, actor)
		req = req.WithContext(ctx)
		w := httptest.NewRecorder()
		x.AddSecret(w, req)
		return w, w.Code
	}

	w, code := sendAdd(pusherDid, addInput)
	if code != http.StatusOK {
		t.Fatalf("expected 200 for add secret, got %d (body: %s)", code, w.Body.String())
	}

	w, code = sendAdd(nonPusherDid, addInput)
	if code != http.StatusUnauthorized {
		t.Fatalf("expected 401 for unauthorized add secret, got %d", code)
	}

	sendList := func(actor syntax.DID, repo string) (*httptest.ResponseRecorder, int) {
		req := httptest.NewRequest(http.MethodGet, "/"+tangled.RepoListSecretsNSID+"?repo="+repo, nil)
		ctx := context.WithValue(req.Context(), ActorDid, actor)
		req = req.WithContext(ctx)
		w := httptest.NewRecorder()
		x.ListSecrets(w, req)
		return w, w.Code
	}

	w, code = sendList(pusherDid, addInput.Repo)
	if code != http.StatusOK {
		t.Fatalf("expected 200 for list secrets, got %d (body: %s)", code, w.Body.String())
	}

	var listOut tangled.RepoListSecrets_Output
	if err := json.Unmarshal(w.Body.Bytes(), &listOut); err != nil {
		t.Fatalf("failed to decode list secrets output: %v", err)
	}
	if len(listOut.Secrets) != 1 || listOut.Secrets[0].Key != "MY_SECRET" {
		t.Fatalf("unexpected secrets list: %+v", listOut.Secrets)
	}

	w, code = sendList(nonPusherDid, addInput.Repo)
	if code != http.StatusUnauthorized {
		t.Fatalf("expected 401 for unauthorized list secrets, got %d", code)
	}

	removeInput := tangled.RepoRemoveSecret_Input{
		Repo: addInput.Repo,
		Key:  "MY_SECRET",
	}

	sendRemove := func(actor syntax.DID, input tangled.RepoRemoveSecret_Input) (*httptest.ResponseRecorder, int) {
		body, _ := json.Marshal(input)
		req := httptest.NewRequest(http.MethodPost, "/"+tangled.RepoRemoveSecretNSID, bytes.NewReader(body))
		ctx := context.WithValue(req.Context(), ActorDid, actor)
		req = req.WithContext(ctx)
		w := httptest.NewRecorder()
		x.RemoveSecret(w, req)
		return w, w.Code
	}

	w, code = sendRemove(pusherDid, removeInput)
	if code != http.StatusOK {
		t.Fatalf("expected 200 for remove secret, got %d (body: %s)", code, w.Body.String())
	}

	w, code = sendList(pusherDid, addInput.Repo)
	if code != http.StatusOK {
		t.Fatalf("list secrets failed: %d", code)
	}
	if err := json.Unmarshal(w.Body.Bytes(), &listOut); err != nil {
		t.Fatalf("failed to decode: %v", err)
	}
	if len(listOut.Secrets) != 0 {
		t.Fatal("secret was not removed")
	}
}
