package markup

import (
	"testing"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func TestParseBlobURI(t *testing.T) {
	tests := []struct {
		name    string
		src     string
		wantDid syntax.DID
		wantCid syntax.CID
		wantOk  bool
	}{
		{
			name:    "valid plc",
			src:     "blob+at://did:plc:abc123/bafyreiabc",
			wantDid: "did:plc:abc123",
			wantCid: "bafyreiabc",
			wantOk:  true,
		},
		{
			name:    "valid web",
			src:     "blob+at://did:web:example.com/bafyreiabc",
			wantDid: "did:web:example.com",
			wantCid: "bafyreiabc",
			wantOk:  true,
		},
		{
			name:   "missing cid",
			src:    "blob+at://did:plc:abc123",
			wantOk: false,
		},
		{
			name:   "empty cid",
			src:    "blob+at://did:plc:abc123/",
			wantOk: false,
		},
		{
			name:   "empty did",
			src:    "blob+at:///bafyreiabc",
			wantOk: false,
		},
		{
			name:   "wrong scheme",
			src:    "https://example.com/image.png",
			wantOk: false,
		},
		{
			name:   "bare at scheme",
			src:    "at://did:plc:abc123/bafyreiabc",
			wantOk: false,
		},
		{
			name:   "empty",
			src:    "",
			wantOk: false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			did, cid, ok := parseBlobURI(tt.src)
			if ok != tt.wantOk {
				t.Fatalf("parseBlobURI(%q) ok = %v, want %v", tt.src, ok, tt.wantOk)
			}
			if !tt.wantOk {
				return
			}
			if did != tt.wantDid || cid != tt.wantCid {
				t.Fatalf("parseBlobURI(%q) = (%q, %q), want (%q, %q)", tt.src, did, cid, tt.wantDid, tt.wantCid)
			}
		})
	}
}

// A nil resolver must never panic and must signal "not rewritten".
func TestBlobToGetBlobURLNilResolver(t *testing.T) {
	rctx := &RenderContext{}
	src := "blob+at://did:plc:abc123/bafyreiabc"
	got, ok := rctx.blobToGetBlobURL(src)
	if ok {
		t.Fatalf("expected ok=false with nil resolver")
	}
	if got != src {
		t.Fatalf("expected src returned unchanged, got %q", got)
	}
}
