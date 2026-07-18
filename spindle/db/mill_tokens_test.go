package db

import (
	"slices"
	"testing"
	"time"
)

func TestAddExecutorTokenRejectsDuplicateName(t *testing.T) {
	d := newTestDB(t)

	if err := d.AddExecutorToken("exec-1", "hash-a", nil, nil); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}
	if err := d.AddExecutorToken("exec-1", "hash-b", nil, nil); err == nil {
		t.Fatal("AddExecutorToken re-registered an existing name; a duplicate must be rejected")
	}

	name, _, ok, err := d.ResolveExecutorToken("hash-a")
	if err != nil {
		t.Fatalf("ResolveExecutorToken(hash-a): %v", err)
	}
	if !ok || name != "exec-1" {
		t.Fatalf("ResolveExecutorToken(hash-a) = (%q, %v), want (exec-1, true)", name, ok)
	}
	if _, _, ok, _ := d.ResolveExecutorToken("hash-b"); ok {
		t.Fatal("rejected duplicate's token resolved; the failed insert leaked a credential")
	}
}

func TestResolveExecutorTokenMissAndHit(t *testing.T) {
	d := newTestDB(t)

	name, _, ok, err := d.ResolveExecutorToken("no-such-hash")
	if err != nil {
		t.Fatalf("ResolveExecutorToken(miss): %v", err)
	}
	if ok || name != "" {
		t.Fatalf("ResolveExecutorToken(miss) = (%q, %v), want (\"\", false)", name, ok)
	}

	if err := d.AddExecutorToken("exec-1", "hash-1", nil, nil); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}

	name, _, ok, err = d.ResolveExecutorToken("hash-1")
	if err != nil {
		t.Fatalf("ResolveExecutorToken(hit): %v", err)
	}
	if !ok || name != "exec-1" {
		t.Fatalf("ResolveExecutorToken(hash-1) = (%q, %v), want (exec-1, true)", name, ok)
	}

	if _, _, ok, _ := d.ResolveExecutorToken("hash-unregistered"); ok {
		t.Fatal("ResolveExecutorToken matched an unregistered hash")
	}
}

func TestRevokeExecutorToken(t *testing.T) {
	d := newTestDB(t)

	if err := d.AddExecutorToken("exec-1", "hash-1", nil, nil); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}

	deleted, err := d.RevokeExecutorToken("exec-1")
	if err != nil {
		t.Fatalf("RevokeExecutorToken: %v", err)
	}
	if !deleted {
		t.Fatal("RevokeExecutorToken reported no deletion for an existing identity")
	}

	if _, _, ok, _ := d.ResolveExecutorToken("hash-1"); ok {
		t.Fatal("revoked token still resolves; revocation is not enforced")
	}

	if deleted, err := d.RevokeExecutorToken("exec-1"); err != nil || deleted {
		t.Fatalf("RevokeExecutorToken(already-gone) = (%v, %v), want (false, nil)", deleted, err)
	}

	if deleted, err := d.RevokeExecutorToken("ghost"); err != nil || deleted {
		t.Fatalf("RevokeExecutorToken(unknown) = (%v, %v), want (false, nil)", deleted, err)
	}
}

func TestExecutorQuarantineIsVisibleAndReversible(t *testing.T) {
	d := newTestDB(t)
	if err := d.AddExecutorToken("exec-1", "hash-1", nil, nil); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}
	if err := d.QuarantineExecutor("exec-1", "missed cancel deadline"); err != nil {
		t.Fatalf("QuarantineExecutor: %v", err)
	}
	if _, _, ok, err := d.ResolveExecutorToken("hash-1"); err != nil || ok {
		t.Fatalf("ResolveExecutorToken(quarantined) = (ok=%v, err=%v), want (false, nil)", ok, err)
	}
	tokens, err := d.ListExecutorTokens()
	if err != nil {
		t.Fatalf("ListExecutorTokens: %v", err)
	}
	if len(tokens) != 1 || tokens[0].QuarantineReason == nil || *tokens[0].QuarantineReason != "missed cancel deadline" || tokens[0].QuarantinedAt == nil {
		t.Fatalf("quarantined token not surfaced: %+v", tokens)
	}
	if cleared, err := d.ClearExecutorQuarantine("exec-1"); err != nil || !cleared {
		t.Fatalf("ClearExecutorQuarantine = (%v, %v), want (true, nil)", cleared, err)
	}
	if _, _, ok, err := d.ResolveExecutorToken("hash-1"); err != nil || !ok {
		t.Fatalf("ResolveExecutorToken(cleared) = (ok=%v, err=%v), want (true, nil)", ok, err)
	}
	if cleared, err := d.ClearExecutorQuarantine("missing"); err != nil || cleared {
		t.Fatalf("ClearExecutorQuarantine(missing) = (%v, %v), want (false, nil)", cleared, err)
	}
}

func TestListExecutorTokens(t *testing.T) {
	d := newTestDB(t)

	tokens, err := d.ListExecutorTokens()
	if err != nil {
		t.Fatalf("ListExecutorTokens(empty): %v", err)
	}
	if len(tokens) != 0 {
		t.Fatalf("ListExecutorTokens on empty table = %d rows, want 0", len(tokens))
	}

	// out of alphabetical order, the query has to sort them
	for _, name := range []string{"charlie", "alice", "bob"} {
		if err := d.AddExecutorToken(name, "hash-"+name, nil, nil); err != nil {
			t.Fatalf("AddExecutorToken(%s): %v", name, err)
		}
	}

	tokens, err = d.ListExecutorTokens()
	if err != nil {
		t.Fatalf("ListExecutorTokens: %v", err)
	}
	want := []string{"alice", "bob", "charlie"}
	if len(tokens) != len(want) {
		t.Fatalf("ListExecutorTokens = %d rows, want %d", len(tokens), len(want))
	}
	for i := range want {
		if tokens[i].Name != want[i] {
			t.Fatalf("ListExecutorTokens[%d].Name = %q, want %q (ordered by name)", i, tokens[i].Name, want[i])
		}
	}
}

// expired tokens must fail closed
func TestResolveExecutorTokenExpiry(t *testing.T) {
	cases := []struct {
		name     string
		seqno    time.Duration
		noExpiry bool
		wantOK   bool
	}{
		{"future expiry resolves", time.Hour, false, true},
		{"past expiry fails closed", -time.Hour, false, false},
		{"nil expiry never expires", 0, true, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			d := newTestDB(t)

			var expires *time.Time
			if !tc.noExpiry {
				exp := time.Now().Add(tc.seqno)
				expires = &exp
			}
			if err := d.AddExecutorToken("exec-1", "hash-1", expires, nil); err != nil {
				t.Fatalf("AddExecutorToken: %v", err)
			}

			name, _, ok, err := d.ResolveExecutorToken("hash-1")
			if err != nil {
				t.Fatalf("ResolveExecutorToken: %v", err)
			}
			if ok != tc.wantOK {
				t.Fatalf("ResolveExecutorToken ok = %v, want %v", ok, tc.wantOK)
			}
			if tc.wantOK && name != "exec-1" {
				t.Fatalf("ResolveExecutorToken name = %q, want exec-1", name)
			}
			if !tc.wantOK && name != "" {
				t.Fatalf("ResolveExecutorToken name = %q, want \"\" when failing closed", name)
			}
		})
	}
}

func TestResolveExecutorTokenRejectsMalformedExpiry(t *testing.T) {
	d := newTestDB(t)
	if _, err := d.Exec(
		`insert into mill_executors (name, token_hash, expires_at) values (?, ?, ?)`,
		"exec-1", "hash-1", "not-a-timestamp",
	); err != nil {
		t.Fatalf("insert malformed token: %v", err)
	}

	name, _, ok, err := d.ResolveExecutorToken("hash-1")
	if err == nil {
		t.Fatal("ResolveExecutorToken accepted a malformed non-NULL expiry")
	}
	if ok || name != "" {
		t.Fatalf("ResolveExecutorToken = (%q, %v, %v), want (\"\", false, error)", name, ok, err)
	}
}

// expiry storage is RFC3339 second precision UTC so
// round trips compare to the second
func TestListExecutorTokensSurfacesExpiry(t *testing.T) {
	d := newTestDB(t)

	exp := time.Now().Add(24 * time.Hour)
	if err := d.AddExecutorToken("expiring", "hash-exp", &exp, nil); err != nil {
		t.Fatalf("AddExecutorToken(expiring): %v", err)
	}
	if err := d.AddExecutorToken("forever", "hash-forever", nil, nil); err != nil {
		t.Fatalf("AddExecutorToken(forever): %v", err)
	}

	tokens, err := d.ListExecutorTokens()
	if err != nil {
		t.Fatalf("ListExecutorTokens: %v", err)
	}

	got := make(map[string]*time.Time, len(tokens))
	for _, tok := range tokens {
		got[tok.Name] = tok.ExpiresAt
	}

	e, present := got["forever"]
	if !present {
		t.Fatal("ListExecutorTokens omitted the non-expiring identity")
	}
	if e != nil {
		t.Fatalf("forever.ExpiresAt = %v, want nil (never expires)", e)
	}

	e, present = got["expiring"]
	if !present {
		t.Fatal("ListExecutorTokens omitted the expiring identity")
	}
	if e == nil {
		t.Fatal("expiring.ExpiresAt = nil, want the stored expiry")
	}
	if e.Unix() != exp.Unix() {
		t.Fatalf("expiring.ExpiresAt = %d (unix), want %d", e.Unix(), exp.Unix())
	}
}

func TestExecutorTokenLabels(t *testing.T) {
	d := newTestDB(t)

	labels := []string{"  foo ", "bar", " foo", ""}
	if err := d.AddExecutorToken("exec-1", "hash-1", nil, labels); err != nil {
		t.Fatalf("AddExecutorToken: %v", err)
	}

	name, resolvedLabels, ok, err := d.ResolveExecutorToken("hash-1")
	if err != nil {
		t.Fatalf("ResolveExecutorToken: %v", err)
	}
	if !ok || name != "exec-1" {
		t.Fatalf("ResolveExecutorToken: ok=%v, name=%q, want true, exec-1", ok, name)
	}

	wantLabels := []string{"bar", "foo"}
	if !slices.Equal(resolvedLabels, wantLabels) {
		t.Fatalf("resolved labels = %v, want %v", resolvedLabels, wantLabels)
	}
}
