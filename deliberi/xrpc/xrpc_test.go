package xrpc

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/bluesky-social/indigo/atproto/auth"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	config "tangled.org/core/deliberi/config"
	db "tangled.org/core/deliberi/db"
	"tangled.org/core/deliberi/models"
	"tangled.org/core/orm"
	"tangled.org/core/xrpc/serviceauth"
)

const (
	testActor    = "did:plc:tester"
	testAudience = "did:web:test.example"
)

// newTestXrpc builds an Xrpc backed by a fresh temp DB, service auth wired to a
// mock directory holding testActor's key. returns router, db, and a token
// signer for a given lexicon method.
func newTestXrpc(t *testing.T) (http.Handler, *db.DB, func(nsid string) string) {
	t.Helper()

	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })

	priv, err := atcrypto.GeneratePrivateKeyP256()
	if err != nil {
		t.Fatalf("generate key: %v", err)
	}
	pub, err := priv.PublicKey()
	if err != nil {
		t.Fatalf("derive pubkey: %v", err)
	}

	dir := identity.NewMockDirectory()
	dir.Insert(identity.Identity{
		DID: syntax.DID(testActor),
		Keys: map[string]identity.VerificationMethod{
			"atproto": {Type: "Multikey", PublicKeyMultibase: pub.Multibase()},
		},
	})

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	x := &Xrpc{
		DB:          d,
		Config:      &config.Config{},
		Logger:      logger,
		ServiceAuth: serviceauth.NewServiceAuth(logger, dir, testAudience),
	}

	sign := func(nsid string) string {
		lxm := syntax.NSID(nsid)
		token, err := auth.SignServiceAuth(syntax.DID(testActor), testAudience, time.Minute, &lxm, priv)
		if err != nil {
			t.Fatalf("sign service auth: %v", err)
		}
		return token
	}

	return x.Router(), d, sign
}

func TestHealth(t *testing.T) {
	router, _, _ := newTestXrpc(t)

	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/_health", nil))

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}
	var body map[string]string
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode: %v; body=%s", err, rec.Body.String())
	}
	if body["version"] == "" {
		t.Fatalf("missing version in %s", rec.Body.String())
	}
}

func TestServiceAuthRequired(t *testing.T) {
	router, _, _ := newTestXrpc(t)

	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/org.tangled.temp.notification.getUnreadCount", nil))

	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 without a service-auth token", rec.Code)
	}
}

func TestWrongLexiconTokenRejected(t *testing.T) {
	router, _, sign := newTestXrpc(t)

	// a token minted for a different method must not authorize this call
	req := httptest.NewRequest(http.MethodGet, "/org.tangled.temp.notification.getUnreadCount", nil)
	req.Header.Set("Authorization", "Bearer "+sign("org.tangled.temp.notification.listNotifications"))
	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, req)

	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 for a token bound to a different method", rec.Code)
	}
}

func TestNotificationGetUnreadCount(t *testing.T) {
	router, d, sign := newTestXrpc(t)

	nsid := "org.tangled.temp.notification.getUnreadCount"
	call := func() int {
		req := httptest.NewRequest(http.MethodGet, "/"+nsid, nil)
		req.Header.Set("Authorization", "Bearer "+sign(nsid))
		rec := httptest.NewRecorder()
		router.ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
		}
		var out struct {
			Count int `json:"count"`
		}
		if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
			t.Fatalf("decode: %v; body=%s", err, rec.Body.String())
		}
		return out.Count
	}

	if got := call(); got != 0 {
		t.Fatalf("empty db count = %d, want 0", got)
	}

	uri := "at://did:plc:repo/sh.tangled.repo.issue/abc"
	if err := db.CreateNotification(d, &models.Notification{
		RecipientDid: testActor,
		AtUri:        uri,
		Type:         models.NotificationTypeIssueCreated,
		ActorDid:     "did:plc:someone",
	}); err != nil {
		t.Fatalf("CreateNotification: %v", err)
	}

	if got := call(); got != 1 {
		t.Fatalf("count after one unread = %d, want 1", got)
	}

	if err := db.MarkRead(d, testActor, uri, true); err != nil {
		t.Fatalf("MarkRead: %v", err)
	}
	if got := call(); got != 0 {
		t.Fatalf("count after marking read = %d, want 0", got)
	}
}

func TestNotificationList(t *testing.T) {
	router, d, sign := newTestXrpc(t)

	uri := "at://did:plc:repo/sh.tangled.repo.issue/abc"
	if err := db.CreateNotification(d, &models.Notification{
		RecipientDid: testActor,
		AtUri:        uri,
		Type:         models.NotificationTypeIssueCreated,
		ActorDid:     "did:plc:someone",
		RepoDid:      "did:plc:repo",
		EntityAt:     uri,
		EntityTitle:  "a bug report",
	}); err != nil {
		t.Fatalf("CreateNotification: %v", err)
	}

	list := func(query string) tangled.TempNotificationListNotifications_Output {
		nsid := "org.tangled.temp.notification.listNotifications"
		req := httptest.NewRequest(http.MethodGet, "/"+nsid+query, nil)
		req.Header.Set("Authorization", "Bearer "+sign(nsid))
		rec := httptest.NewRecorder()
		router.ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
		}
		var out tangled.TempNotificationListNotifications_Output
		if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
			t.Fatalf("decode: %v; body=%s", err, rec.Body.String())
		}
		return out
	}

	out := list("")
	if len(out.Notifications) != 1 {
		t.Fatalf("got %d notifications, want 1", len(out.Notifications))
	}
	n := out.Notifications[0]
	if n.Uri != uri {
		t.Fatalf("uri = %q, want %q", n.Uri, uri)
	}
	if n.Type != string(models.NotificationTypeIssueCreated) {
		t.Fatalf("type = %q, want issue_created", n.Type)
	}
	if n.Category != "work" {
		t.Fatalf("category = %q, want work", n.Category)
	}
	if n.RepoDid == nil || *n.RepoDid != "did:plc:repo" {
		t.Fatalf("repoDid = %v, want did:plc:repo", n.RepoDid)
	}
	if n.IssueAt == nil || *n.IssueAt != uri {
		t.Fatalf("issueAt = %v, want %q", n.IssueAt, uri)
	}
	if out.WorkUnreadCount != 1 {
		t.Fatalf("workUnreadCount = %d, want 1", out.WorkUnreadCount)
	}

	if err := db.MarkRead(d, testActor, uri, true); err != nil {
		t.Fatalf("MarkRead: %v", err)
	}
	if out := list("?read=unread"); len(out.Notifications) != 0 {
		t.Fatalf("unread list after read = %d, want 0", len(out.Notifications))
	}
}

func TestUpdateSeenPersists(t *testing.T) {
	router, d, sign := newTestXrpc(t)

	uri := "at://did:plc:repo/sh.tangled.repo.issue/xyz"
	if err := db.CreateNotification(d, &models.Notification{
		RecipientDid: testActor,
		AtUri:        uri,
		Type:         models.NotificationTypeIssueCreated,
		ActorDid:     "did:plc:someone",
	}); err != nil {
		t.Fatalf("CreateNotification: %v", err)
	}

	nsid := "org.tangled.temp.notification.updateSeen"
	req := httptest.NewRequest(http.MethodPost, "/"+nsid, strings.NewReader(`{"uri":"`+uri+`","read":true}`))
	req.Header.Set("Authorization", "Bearer "+sign(nsid))
	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}

	unread, err := db.CountNotifications(d, testActor, orm.FilterEq("read", 0))
	if err != nil {
		t.Fatalf("CountNotifications: %v", err)
	}
	if unread != 0 {
		t.Fatalf("unread after updateSeen = %d, want 0", unread)
	}
}
