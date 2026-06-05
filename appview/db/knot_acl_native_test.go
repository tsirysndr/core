package db

import (
	"context"
	"testing"
)

func TestKnotAclNativeDefaultsFalse(t *testing.T) {
	d := newTestDB(t)

	native, err := IsKnotAclNative(context.Background(), d, "clam.nel.pet")
	if err != nil {
		t.Fatalf("IsKnotAclNative: %v", err)
	}
	if native {
		t.Fatal("an unseen knot must default to not native")
	}
}

func TestKnotAclNativeMarkLatches(t *testing.T) {
	d := newTestDB(t)

	if err := MarkKnotAclNative(context.Background(), d, "whelk.nel.pet"); err != nil {
		t.Fatalf("MarkKnotAclNative: %v", err)
	}

	native, err := IsKnotAclNative(context.Background(), d, "whelk.nel.pet")
	if err != nil {
		t.Fatalf("IsKnotAclNative: %v", err)
	}
	if !native {
		t.Fatal("a marked knot must read back native")
	}
}

func TestKnotAclNativeMarkIsIdempotent(t *testing.T) {
	d := newTestDB(t)

	if err := MarkKnotAclNative(context.Background(), d, "limpet.nel.pet"); err != nil {
		t.Fatalf("first mark: %v", err)
	}
	if err := MarkKnotAclNative(context.Background(), d, "limpet.nel.pet"); err != nil {
		t.Fatalf("second mark must be a no-op, got: %v", err)
	}

	if n := countRows(t, d, `select count(*) from knot_acl_native where domain = ?`, "limpet.nel.pet"); n != 1 {
		t.Fatalf("rows = %d, want exactly 1 after a repeated mark", n)
	}
}
