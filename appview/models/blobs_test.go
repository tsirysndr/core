package models

import (
	"encoding/json"
	"testing"

	"github.com/ipfs/go-cid"

	lexutil "github.com/bluesky-social/indigo/lex/util"
)

// two distinct, valid CIDv1 strings for use as blob refs
const (
	cidA = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"
	cidB = "bafybeihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku"
)

func blobJSON(t *testing.T, cidStr, mime string) string {
	t.Helper()
	c, err := cid.Decode(cidStr)
	if err != nil {
		t.Fatalf("decode cid: %v", err)
	}
	b := lexutil.LexBlob{Ref: lexutil.LexLink(c), MimeType: mime, Size: 1234}
	raw, err := json.Marshal(b)
	if err != nil {
		t.Fatalf("marshal blob: %v", err)
	}
	return string(raw)
}

func cids(blobs []*lexutil.LexBlob) []string {
	out := make([]string, len(blobs))
	for i, b := range blobs {
		out[i] = b.Ref.String()
	}
	return out
}

func TestParseBlobs(t *testing.T) {
	ja := blobJSON(t, cidA, "image/png")
	jb := blobJSON(t, cidB, "image/jpeg")
	body := "look: ![a](blob+at://did:plc:abc/" + cidA + ") and nothing else"

	t.Run("keeps referenced, drops unreferenced", func(t *testing.T) {
		got := ParseBlobs([]string{ja, jb}, body)
		if len(got) != 1 || got[0].Ref.String() != cidA {
			t.Fatalf("got %v, want [%s]", cids(got), cidA)
		}
	})

	t.Run("dedups repeated cid", func(t *testing.T) {
		got := ParseBlobs([]string{ja, ja}, body)
		if len(got) != 1 {
			t.Fatalf("got %d blobs, want 1", len(got))
		}
	})

	t.Run("ignores malformed json", func(t *testing.T) {
		got := ParseBlobs([]string{"not json", ja}, body)
		if len(got) != 1 {
			t.Fatalf("got %d blobs, want 1", len(got))
		}
	})

	t.Run("empty input", func(t *testing.T) {
		if got := ParseBlobs(nil, body); got != nil {
			t.Fatalf("got %v, want nil", cids(got))
		}
	})
}

func TestMergeBlobs(t *testing.T) {
	ja := blobJSON(t, cidA, "image/png")
	jb := blobJSON(t, cidB, "image/jpeg")

	ca, _ := cid.Decode(cidA)
	existing := []*lexutil.LexBlob{{Ref: lexutil.LexLink(ca), MimeType: "image/png", Size: 1234}}

	// body references both A (existing) and B (new upload)
	body := "![a](blob+at://did:plc:abc/" + cidA + ") ![b](blob+at://did:plc:abc/" + cidB + ")"

	t.Run("union of existing and new, both referenced", func(t *testing.T) {
		got := ParseBlobsSet(MergeBlobs(existing, []string{jb}, body))
		if !got[cidA] || !got[cidB] || len(got) != 2 {
			t.Fatalf("got %v, want {%s,%s}", got, cidA, cidB)
		}
	})

	t.Run("drops existing no longer in body", func(t *testing.T) {
		bodyOnlyB := "![b](blob+at://did:plc:abc/" + cidB + ")"
		got := MergeBlobs(existing, []string{jb}, bodyOnlyB)
		if len(got) != 1 || got[0].Ref.String() != cidB {
			t.Fatalf("got %v, want [%s]", cids(got), cidB)
		}
	})

	t.Run("no double-count when new equals existing", func(t *testing.T) {
		got := MergeBlobs(existing, []string{ja}, body)
		if len(got) != 1 || got[0].Ref.String() != cidA {
			t.Fatalf("got %v, want [%s]", cids(got), cidA)
		}
	})
}

func ParseBlobsSet(blobs []*lexutil.LexBlob) map[string]bool {
	m := make(map[string]bool, len(blobs))
	for _, b := range blobs {
		m[b.Ref.String()] = true
	}
	return m
}
