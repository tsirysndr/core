package eventconsumer

import (
	"testing"

	"tangled.org/core/eventconsumer/cursor"
)

func TestMigrateLegacyCursor(t *testing.T) {
	const host = "whelk.knot.tld"
	src := NewKnotSource(host)

	t.Run("copies legacy bare-host cursor to namespaced key", func(t *testing.T) {
		store := &cursor.MemoryStore{}
		store.Set(host, 42)

		MigrateLegacyCursor(store, src)

		if got := store.Get(src.Key()); got != 42 {
			t.Fatalf("namespaced key = %d, want 42", got)
		}
		if got := store.Get(host); got != 42 {
			t.Fatalf("legacy key = %d, want it left at 42", got)
		}
	})

	t.Run("does not pave over an already-migrated cursor", func(t *testing.T) {
		store := &cursor.MemoryStore{}
		store.Set(src.Key(), 100)
		store.Set(host, 42)

		MigrateLegacyCursor(store, src)

		if got := store.Get(src.Key()); got != 100 {
			t.Fatalf("namespaced key = %d, want 100", got)
		}
	})

	t.Run("noop when neither key is set", func(t *testing.T) {
		store := &cursor.MemoryStore{}

		MigrateLegacyCursor(store, src)

		if got := store.Get(src.Key()); got != 0 {
			t.Fatalf("namespaced key = %d, want 0", got)
		}
	})

	t.Run("namespaces the same host by kind", func(t *testing.T) {
		const shared = "mussel.knot.tld"
		knot := NewKnotSource(shared)
		spindle := NewSpindleSource(shared)

		store := &cursor.MemoryStore{}
		store.Set(shared, 500)

		MigrateLegacyCursor(store, knot)
		MigrateLegacyCursor(store, spindle)

		if got := store.Get(knot.Key()); got != 500 {
			t.Fatalf("knot key = %d, want 500", got)
		}
		if got := store.Get(spindle.Key()); got != 500 {
			t.Fatalf("spindle key = %d, want 500", got)
		}
	})
}
