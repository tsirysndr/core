package types

import (
	"strings"
	"testing"
	"time"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
)

func TestPayloadRootCommit(t *testing.T) {
	when := time.Date(2025, 1, 1, 12, 0, 0, 0, time.UTC)
	sig := object.Signature{Name: "Alice", Email: "alice@example.com", When: when}

	c := Commit{
		Author:    sig,
		Committer: sig,
		Message:   "initial commit\n",
		Tree:      "abc123",
	}

	payload := c.Payload()

	for _, line := range strings.Split(payload, "\n") {
		if strings.HasPrefix(line, "parent ") || line == "parent" {
			t.Errorf("root commit payload must not contain a parent line, got: %q\nfull payload:\n%s", line, payload)
		}
	}
}

func TestPayloadWithParentHashes(t *testing.T) {
	when := time.Date(2025, 1, 1, 12, 0, 0, 0, time.UTC)
	sig := object.Signature{Name: "Alice", Email: "alice@example.com", When: when}
	parent := plumbing.NewHash("0000000000000000000000000000000000000001")

	c := Commit{
		Author:       sig,
		Committer:    sig,
		Message:      "second commit\n",
		Tree:         "abc123",
		ParentHashes: []plumbing.Hash{parent},
	}

	payload := c.Payload()
	want := "parent " + parent.String()
	if !strings.Contains(payload, want) {
		t.Errorf("payload missing %q\nfull payload:\n%s", want, payload)
	}
}

func TestPayloadLegacyParentField(t *testing.T) {
	when := time.Date(2025, 1, 1, 12, 0, 0, 0, time.UTC)
	sig := object.Signature{Name: "Alice", Email: "alice@example.com", When: when}

	c := Commit{
		Author:    sig,
		Committer: sig,
		Message:   "second commit\n",
		Tree:      "abc123",
		Parent:    "0000000000000000000000000000000000000001",
	}

	payload := c.Payload()
	want := "parent 0000000000000000000000000000000000000001"
	if !strings.Contains(payload, want) {
		t.Errorf("payload missing %q\nfull payload:\n%s", want, payload)
	}
}
