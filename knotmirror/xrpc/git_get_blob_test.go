package xrpc

import (
	"crypto/sha256"
	"encoding/hex"
	"regexp"
	"testing"
)

func TestBlobETag(t *testing.T) {
	contents := []byte("kelp and periwinkle\n")
	eTag := blobETag(contents)
	bare := sha256.Sum256(contents)

	if !regexp.MustCompile(`^"[0-9a-f]{64}"$`).MatchString(eTag) {
		t.Errorf("etag = %s, want a quoted sha256 digest", eTag)
	}
	if eTag == `"`+hex.EncodeToString(bare[:])+`"` {
		t.Error("the mirror must separate its validator from a bare content digest, because a knot answering the same proxied request hashes the same bytes")
	}
	if same, other := blobETag(contents), blobETag([]byte("conch\n")); same != eTag || other == eTag {
		t.Errorf("etag = %s, then %s for the same blob and %s for another, want one validator per blob", eTag, same, other)
	}
}
