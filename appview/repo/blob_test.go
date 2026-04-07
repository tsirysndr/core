package repo

import (
	"mime"
	"strings"
	"testing"
)

func TestSafeBinaryMIMEType(t *testing.T) {
	allowed := []string{
		"image/png",
		"image/jpeg",
		"image/gif",
		"image/webp",
		"image/avif",
		"video/mp4",
		"video/webm",
		"video/ogg",
	}
	for _, ct := range allowed {
		if !safeBinaryMIMEType(ct) {
			t.Errorf("expected %q to be allowed, but it was not", ct)
		}
	}

	rejected := []string{
		// SVG must be rejected — it supports embedded scripts.
		"image/svg+xml",
		// Other XML-based or scriptable types.
		"image/svg",
		"application/pdf",
		"application/octet-stream",
		"text/html",
		"text/javascript",
		// Empty / garbage.
		"",
		"image/",
		"video/",
	}
	for _, ct := range rejected {
		if safeBinaryMIMEType(ct) {
			t.Errorf("expected %q to be rejected, but it was allowed", ct)
		}
	}
}

// TestBlobMIMENormalization verifies that mime.ParseMediaType strips
// parameters before classification, closing bypass attempts such as
// "image/svg+xml; charset=utf-8".
func TestBlobMIMENormalization(t *testing.T) {
	cases := []struct {
		raw            string
		wantSafeBinary bool
		wantTextual    bool
	}{
		// Parameters must not smuggle SVG past the allowlist.
		{"image/svg+xml; charset=utf-8", false, false},
		{"image/svg+xml; innocent=param", false, false},
		// Parameters on safe types should still be allowed.
		{"image/png; q=0.9", true, false},
		// Parameters on textual types.
		{"text/plain; charset=utf-8", false, true},
		{"application/json; charset=utf-8", false, true},
	}

	for _, tc := range cases {
		mediaType, _, _ := mime.ParseMediaType(tc.raw)
		gotSafeBinary := safeBinaryMIMEType(mediaType)
		gotTextual := strings.HasPrefix(mediaType, "text/") || isTextualMimeType(mediaType)

		if gotSafeBinary != tc.wantSafeBinary {
			t.Errorf("safeBinaryMIMEType(%q): got %v, want %v (parsed as %q)",
				tc.raw, gotSafeBinary, tc.wantSafeBinary, mediaType)
		}
		if gotTextual != tc.wantTextual {
			t.Errorf("isTextual(%q): got %v, want %v (parsed as %q)",
				tc.raw, gotTextual, tc.wantTextual, mediaType)
		}
	}
}
