package xrpc

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/bluesky-social/indigo/atproto/auth"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/xrpc/serviceauth"
)

const (
	testActor    = "did:plc:tester"
	testAudience = "did:web:test.example"
)

// newTestXrpc builds an Xrpc backed by a fresh temp DB, with service auth wired
// to a mock directory holding testActor's key. It returns the router, the DB,
// and a function that signs a service-auth token for a given lexicon method.
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

	if err := db.CreateNotification(d, &models.Notification{
		RecipientDid: testActor,
		ActorDid:     "did:plc:someone",
		Type:         models.NotificationTypeRepoStarred,
		Read:         false,
	}); err != nil {
		t.Fatalf("CreateNotification: %v", err)
	}

	if got := call(); got != 1 {
		t.Fatalf("count after one unread = %d, want 1", got)
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
