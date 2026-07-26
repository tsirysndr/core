package models

import (
	"encoding/json"
	"strings"

	lexutil "github.com/bluesky-social/indigo/lex/util"
)

// decodeBlob parses a single JSON-encoded LexBlob as submitted by the markdown
// editor. Returns ok=false for malformed JSON or a blob without a CID.
func decodeBlob(s string) (*lexutil.LexBlob, bool) {
	var b lexutil.LexBlob
	if err := json.Unmarshal([]byte(s), &b); err != nil {
		return nil, false
	}
	if !b.Ref.Defined() {
		return nil, false
	}
	return &b, true
}

// ParseBlobs decodes the blob refs submitted by the editor, keeping only those
// still referenced in body. Referencing them on the record pins them against
// PDS garbage collection; the body filter drops orphans the user removed.
func ParseBlobs(raw []string, body string) []*lexutil.LexBlob {
	return MergeBlobs(nil, raw, body)
}

// MergeBlobs unions already-committed blobs with newly-submitted ones, keeping
// only CIDs still in body. Used on edit to preserve earlier images.
func MergeBlobs(existing []*lexutil.LexBlob, raw []string, body string) []*lexutil.LexBlob {
	seen := make(map[string]struct{})
	var out []*lexutil.LexBlob

	keep := func(b *lexutil.LexBlob) {
		if b == nil || !b.Ref.Defined() {
			return
		}
		cid := b.Ref.String()
		if _, dup := seen[cid]; dup {
			return
		}
		if !strings.Contains(body, cid) {
			return
		}
		seen[cid] = struct{}{}
		out = append(out, b)
	}

	for _, b := range existing {
		keep(b)
	}
	for _, s := range raw {
		if b, ok := decodeBlob(s); ok {
			keep(b)
		}
	}

	return out
}
