package oauth

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/config"
	"tangled.org/core/consts"
)

type fakeAcl struct {
	member  bool
	gotHost string
	gotDid  string
}

func (f *fakeAcl) InvalidateMembers(host string) {}

func (f *fakeAcl) IsKnotMember(ctx context.Context, host, userDid string) bool {
	f.gotHost = host
	f.gotDid = userDid
	return f.member
}

func TestAddToDefaultKnot_ShortCircuitsWhenAlreadyMember(t *testing.T) {
	acl := &fakeAcl{member: true}
	o := &OAuth{
		Acl:    acl,
		Logger: slog.New(slog.NewTextHandler(io.Discard, nil)),
		Config: &config.Config{
			Core: config.CoreConfig{Dev: true},
			Knot: config.KnotConfig{Default: consts.DefaultKnot},
		},
	}

	o.addToDefaultKnot(syntax.DID("did:plc:akshay"))

	if acl.gotDid != "did:plc:akshay" {
		t.Fatalf("IsKnotMember did = %q, want did:plc:akshay", acl.gotDid)
	}
	if acl.gotHost != consts.DefaultKnot {
		t.Fatalf("IsKnotMember host = %q, want %q", acl.gotHost, consts.DefaultKnot)
	}
}

func TestAddMemberViaKnotAdmin_HonorsDeadline(t *testing.T) {
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		<-release
	}))
	defer srv.Close()
	defer close(release)

	o := &OAuth{Config: &config.Config{
		Core: config.CoreConfig{Dev: true},
		Knot: config.KnotConfig{AdminSecret: "hunter2"},
	}}

	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	done := make(chan error, 1)
	go func() {
		done <- o.addMemberViaKnotAdmin(ctx, strings.TrimPrefix(srv.URL, "http://"), syntax.DID("did:plc:whelk"))
	}()

	select {
	case err := <-done:
		if err == nil {
			t.Fatal("a hung knot must surface an error, got nil")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("addMemberViaKnotAdmin blocked past its deadline; the request has no timeout")
	}
}

func TestOnboardActionFor(t *testing.T) {
	cases := []struct {
		name  string
		state defaultKnotState
		want  onboardAction
	}{
		{"native default knot with admin secret uses the admin api", defaultKnotState{native: true, adminSecretSet: true}, onboardViaAdminAPI},
		{"native default knot without admin secret is blocked", defaultKnotState{native: true, adminSecretSet: false}, onboardBlockedMissingSecret},
		{"legacy default knot with admin secret skips the legacy record", defaultKnotState{native: false, adminSecretSet: true}, onboardBlockedSecretSet},
		{"legacy default knot without admin secret writes the legacy record", defaultKnotState{native: false, adminSecretSet: false}, onboardViaRecord},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := onboardActionFor(c.state); got != c.want {
				t.Fatalf("onboardActionFor(%+v) = %d, want %d", c.state, got, c.want)
			}
		})
	}
}
