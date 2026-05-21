package state

import (
	"testing"

	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
)

func TestMigrateLegacyCursor_CopiesBareHostToKindKey(t *testing.T) {
	store := &cursor.MemoryStore{}
	store.Set("clam.oyster.cafe", 1700000000123456789)

	migrateLegacyCursor(store, ec.NewKnotSource("clam.oyster.cafe"))

	if got := store.Get("knot:clam.oyster.cafe"); got != 1700000000123456789 {
		t.Fatalf("new key cursor = %d, want legacy value", got)
	}
}

func TestMigrateLegacyCursor_DoesNotClobberAdvancedCursor(t *testing.T) {
	store := &cursor.MemoryStore{}
	store.Set("knot:whelk.oyster.cafe", 999)
	store.Set("whelk.oyster.cafe", 100)

	migrateLegacyCursor(store, ec.NewKnotSource("whelk.oyster.cafe"))

	if got := store.Get("knot:whelk.oyster.cafe"); got != 999 {
		t.Fatalf("new key cursor = %d, want it left untouched at 999", got)
	}
}

func TestMigrateLegacyCursor_NoLegacyIsNoOp(t *testing.T) {
	store := &cursor.MemoryStore{}

	migrateLegacyCursor(store, ec.NewKnotSource("limpet.nel.pet"))

	if got := store.Get("knot:limpet.nel.pet"); got != 0 {
		t.Fatalf("new key cursor = %d, want 0", got)
	}
}

func TestMigrateLegacyCursor_KindsStayNamespaced(t *testing.T) {
	store := &cursor.MemoryStore{}
	store.Set("mussel.oyster.cafe", 500)

	migrateLegacyCursor(store, ec.NewKnotSource("mussel.oyster.cafe"))
	migrateLegacyCursor(store, ec.NewSpindleSource("mussel.oyster.cafe"))

	if got := store.Get("knot:mussel.oyster.cafe"); got != 500 {
		t.Fatalf("knot cursor = %d, want 500", got)
	}
	if got := store.Get("spindle:mussel.oyster.cafe"); got != 500 {
		t.Fatalf("spindle cursor = %d, want 500", got)
	}
}
