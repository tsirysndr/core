package serviceauth

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/bluesky-social/indigo/atproto/auth"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
)

const (
	testIssuer   = "did:plc:boltless"
	testAudience = "did:web:knot.example"
	testLxm      = "sh.tangled.repo.create"
)

func newTestServiceAuth(t *testing.T) (*ServiceAuth, atcrypto.PrivateKey) {
	t.Helper()
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
		DID: syntax.DID(testIssuer),
		Keys: map[string]identity.VerificationMethod{
			"atproto": {Type: "Multikey", PublicKeyMultibase: pub.Multibase()},
		},
	})
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	return NewServiceAuth(logger, dir, testAudience), priv
}

func signed(t *testing.T, priv atcrypto.PrivateKey, lxm *syntax.NSID) string {
	t.Helper()
	token, err := auth.SignServiceAuth(syntax.DID(testIssuer), testAudience, time.Minute, lxm, priv)
	if err != nil {
		t.Fatalf("sign service auth: %v", err)
	}
	return token
}

func serve(sa *ServiceAuth, path, token string) (*httptest.ResponseRecorder, *syntax.DID) {
	var seen *syntax.DID
	next := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if did, ok := r.Context().Value(ActorDid).(syntax.DID); ok {
			seen = &did
		}
		w.WriteHeader(http.StatusOK)
	})
	req := httptest.NewRequest(http.MethodPost, path, nil)
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	rec := httptest.NewRecorder()
	sa.VerifyServiceAuth(next).ServeHTTP(rec, req)
	return rec, seen
}

func TestVerifyServiceAuth_MatchingLxmPasses(t *testing.T) {
	sa, priv := newTestServiceAuth(t)
	lxm := syntax.NSID(testLxm)
	rec, seen := serve(sa, "/xrpc/"+testLxm, signed(t, priv, &lxm))
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200; body=%s", rec.Code, rec.Body.String())
	}
	if seen == nil || seen.String() != testIssuer {
		t.Fatalf("ActorDid = %v, want %s", seen, testIssuer)
	}
}

func TestVerifyServiceAuth_MismatchedLxmRejected(t *testing.T) {
	sa, priv := newTestServiceAuth(t)
	other := syntax.NSID("sh.tangled.knot.addMember")
	rec, _ := serve(sa, "/xrpc/"+testLxm, signed(t, priv, &other))
	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 for lxm bound to a different method", rec.Code)
	}
}

func TestVerifyServiceAuth_NoLxmClaimRejected(t *testing.T) {
	sa, priv := newTestServiceAuth(t)
	rec, _ := serve(sa, "/xrpc/"+testLxm, signed(t, priv, nil))
	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 for a token carrying no lxm claim", rec.Code)
	}
}

func TestVerifyServiceAuth_UnparseablePathRejected(t *testing.T) {
	sa, priv := newTestServiceAuth(t)
	lxm := syntax.NSID(testLxm)
	rec, _ := serve(sa, "/xrpc/notansid", signed(t, priv, &lxm))
	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 when the path tail is not a valid NSID", rec.Code)
	}
}

func TestVerifyServiceAuth_GarbageTokenRejected(t *testing.T) {
	sa, _ := newTestServiceAuth(t)
	rec, _ := serve(sa, "/xrpc/"+testLxm, "not.a.jwt")
	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403 for an unverifiable token", rec.Code)
	}
}
