package db

import (
	"context"
	"fmt"
	"path/filepath"
	"sync"
	"testing"
)

// TestGetOrAssignOwnerUID_Concurrent verifies that concurrent calls for
// distinct DIDs each get a unique UID, with no duplicates from a race on
// the uid_counter table.
func TestGetOrAssignOwnerUID_Concurrent(t *testing.T) {
	d := newTestDB(t)

	const n = 50
	results := make([]uint32, n)
	var wg sync.WaitGroup
	wg.Add(n)
	for i := 0; i < n; i++ {
		go func(i int) {
			defer wg.Done()
			uid, err := d.GetOrAssignOwnerUID(fmt.Sprintf("did:plc:test-%03d", i))
			if err != nil {
				t.Errorf("GetOrAssignOwnerUID(%d): %v", i, err)
				return
			}
			results[i] = uid
		}(i)
	}
	wg.Wait()

	seen := map[uint32]int{}
	for i, uid := range results {
		if uid == 0 {
			t.Errorf("result[%d] is zero (probably an error above)", i)
			continue
		}
		if prev, ok := seen[uid]; ok {
			t.Errorf("uid %d was returned for both index %d and %d", uid, prev, i)
		}
		seen[uid] = i
	}

	if len(seen) != n {
		t.Errorf("got %d unique UIDs, want %d", len(seen), n)
	}
}

// TestGetOrAssignOwnerUID_Idempotent verifies that repeated calls for the
// same DID return the same UID.
func TestGetOrAssignOwnerUID_Idempotent(t *testing.T) {
	d := newTestDB(t)
	const did = "did:plc:stable"

	first, err := d.GetOrAssignOwnerUID(did)
	if err != nil {
		t.Fatalf("first call: %v", err)
	}
	for i := 0; i < 5; i++ {
		uid, err := d.GetOrAssignOwnerUID(did)
		if err != nil {
			t.Fatalf("repeat call %d: %v", i, err)
		}
		if uid != first {
			t.Errorf("repeat call %d returned %d, want %d", i, uid, first)
		}
	}
}

// TestGetOrAssignOwnerUID_StartsAt100000 verifies the counter seeds correctly.
func TestGetOrAssignOwnerUID_StartsAt100000(t *testing.T) {
	d := newTestDB(t)
	uid, err := d.GetOrAssignOwnerUID("did:plc:first")
	if err != nil {
		t.Fatalf("first call: %v", err)
	}
	if uid != 100000 {
		t.Errorf("first UID = %d, want 100000", uid)
	}
}

func newTestDB(t *testing.T) *DB {
	t.Helper()
	d, err := Setup(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("db.Setup: %v", err)
	}
	return d
}
