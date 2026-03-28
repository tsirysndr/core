package crypto

import (
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"strings"
	"testing"

	"github.com/hiddeco/sshsig"
	"golang.org/x/crypto/ssh"
)

// testKey generates an ephemeral ed25519 key pair and returns the ssh.Signer
// and the public key in authorized_keys format.
func testKey(t *testing.T) (ssh.Signer, []byte) {
	t.Helper()
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate ed25519 key: %v", err)
	}
	signer, err := ssh.NewSignerFromKey(priv)
	if err != nil {
		t.Fatalf("create signer: %v", err)
	}
	return signer, ssh.MarshalAuthorizedKey(signer.PublicKey())
}

// testSign signs payload with signer using the same parameters as VerifySignature
// expects (SHA-512, "git" namespace) and returns the armored signature.
func testSign(t *testing.T, signer ssh.Signer, payload []byte) []byte {
	t.Helper()
	sig, err := sshsig.Sign(bytes.NewReader(payload), signer, sshsig.HashSHA512, "git")
	if err != nil {
		t.Fatalf("sign payload: %v", err)
	}
	return sshsig.Armor(sig)
}

func TestSSHFingerprint(t *testing.T) {
	t.Run("valid key returns SHA256 fingerprint", func(t *testing.T) {
		_, pubKeyBytes := testKey(t)
		fp, err := SSHFingerprint(string(pubKeyBytes))
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if !strings.HasPrefix(fp, "SHA256:") {
			t.Errorf("fingerprint %q does not start with SHA256:", fp)
		}
	})

	t.Run("same key returns identical fingerprint", func(t *testing.T) {
		_, pubKeyBytes := testKey(t)
		fp1, _ := SSHFingerprint(string(pubKeyBytes))
		fp2, _ := SSHFingerprint(string(pubKeyBytes))
		if fp1 != fp2 {
			t.Errorf("fingerprint not deterministic: %q != %q", fp1, fp2)
		}
	})

	t.Run("different keys return different fingerprints", func(t *testing.T) {
		_, pub1 := testKey(t)
		_, pub2 := testKey(t)
		fp1, _ := SSHFingerprint(string(pub1))
		fp2, _ := SSHFingerprint(string(pub2))
		if fp1 == fp2 {
			t.Error("different keys produced the same fingerprint")
		}
	})

	t.Run("malformed key returns error", func(t *testing.T) {
		_, err := SSHFingerprint("not a valid ssh public key")
		if err == nil {
			t.Error("expected error for malformed key")
		}
	})
}

func TestVerifySignature(t *testing.T) {
	signer, pubKeyBytes := testKey(t)
	payload := []byte("test payload")
	armoredSig := testSign(t, signer, payload)

	t.Run("valid signature verifies successfully", func(t *testing.T) {
		err, ok := VerifySignature(pubKeyBytes, armoredSig, payload)
		if err != nil {
			t.Errorf("unexpected error: %v", err)
		}
		if !ok {
			t.Error("expected ok=true for valid signature")
		}
	})

	t.Run("malformed public key returns error", func(t *testing.T) {
		err, ok := VerifySignature([]byte("not a valid key"), armoredSig, payload)
		if err == nil {
			t.Error("expected error for malformed public key")
		}
		if ok {
			t.Error("expected ok=false")
		}
	})

	t.Run("malformed signature returns error", func(t *testing.T) {
		err, ok := VerifySignature(pubKeyBytes, []byte("not a valid signature"), payload)
		if err == nil {
			t.Error("expected error for malformed signature")
		}
		if ok {
			t.Error("expected ok=false")
		}
	})

	t.Run("tampered payload fails verification", func(t *testing.T) {
		err, ok := VerifySignature(pubKeyBytes, armoredSig, []byte("tampered"))
		if err == nil {
			t.Error("expected error for tampered payload")
		}
		if ok {
			t.Error("expected ok=false for tampered payload")
		}
	})

	t.Run("wrong public key fails verification", func(t *testing.T) {
		_, otherPubKey := testKey(t)
		err, ok := VerifySignature(otherPubKey, armoredSig, payload)
		if err == nil {
			t.Error("expected error for wrong public key")
		}
		if ok {
			t.Error("expected ok=false for wrong public key")
		}
	})
}
