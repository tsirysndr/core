package knotacl

import (
	"context"
	"path/filepath"
	"testing"

	"tangled.org/core/appview/db"
)

func TestLatchRoundTrip(t *testing.T) {
	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "appview.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}

	l := NewLatch(d, testLogger())

	if l.IsNative("clam.nel.pet") {
		t.Fatal("a fresh host must not read native through the adapter")
	}

	l.MarkNative("clam.nel.pet")

	if !l.IsNative("clam.nel.pet") {
		t.Fatal("a marked host must read back native through the adapter")
	}
	if l.IsNative("whelk.nel.pet") {
		t.Fatal("an unmarked sibling must stay non-native")
	}
}
