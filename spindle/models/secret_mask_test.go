package models

import (
	"encoding/base64"
	"testing"
)

func TestSecretMask_BasicMasking(t *testing.T) {
	mask := NewSecretMask([]string{"mysecret123"})

	input := "The password is mysecret123 in this log"
	expected := "The password is *** in this log"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_Base64Encoded(t *testing.T) {
	secret := "mysecret123"
	mask := NewSecretMask([]string{secret})

	b64 := base64.StdEncoding.EncodeToString([]byte(secret))
	input := "Encoded: " + b64
	expected := "Encoded: ***"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_Base64NoPadding(t *testing.T) {
	// "test" encodes to "dGVzdA==" with padding
	secret := "test"
	mask := NewSecretMask([]string{secret})

	b64NoPad := "dGVzdA" // base64 without padding
	input := "Token: " + b64NoPad
	expected := "Token: ***"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_MultipleSecrets(t *testing.T) {
	mask := NewSecretMask([]string{"password1", "apikey123"})

	input := "Using password1 and apikey123 for auth"
	expected := "Using *** and *** for auth"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_MultipleOccurrences(t *testing.T) {
	mask := NewSecretMask([]string{"secret"})

	input := "secret appears twice: secret"
	expected := "*** appears twice: ***"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_ShortValues(t *testing.T) {
	mask := NewSecretMask([]string{"abc", "xy", ""})

	if mask == nil {
		t.Fatal("expected non-nil mask")
	}

	input := "abc xy test"
	expected := "*** *** test"
	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_NilMask(t *testing.T) {
	var mask *SecretMask

	input := "some input text"
	result := mask.Mask(input)
	if result != input {
		t.Errorf("expected %q, got %q", input, result)
	}
}

func TestSecretMask_EmptyInput(t *testing.T) {
	mask := NewSecretMask([]string{"secret"})

	result := mask.Mask("")
	if result != "" {
		t.Errorf("expected empty string, got %q", result)
	}
}

func TestSecretMask_NoMatch(t *testing.T) {
	mask := NewSecretMask([]string{"secretvalue"})

	input := "nothing to mask here"
	result := mask.Mask(input)
	if result != input {
		t.Errorf("expected %q, got %q", input, result)
	}
}

func TestSecretMask_EmptySecretsList(t *testing.T) {
	mask := NewSecretMask([]string{})

	if mask != nil {
		t.Error("expected nil mask for empty secrets list")
	}
}

func TestSecretMask_EmptySecretsFiltered(t *testing.T) {
	mask := NewSecretMask([]string{"ab", "validpassword", "", "xyz"})

	input := "Using validpassword here"
	expected := "Using *** here"

	result := mask.Mask(input)
	if result != expected {
		t.Errorf("expected %q, got %q", expected, result)
	}
}

func TestSecretMask_MultilineSecretLines(t *testing.T) {
	mask := NewSecretMask([]string{"line-one\nline-two"})

	if result := mask.Mask("line-one\nline-two"); result != "***" {
		t.Errorf("full multiline secret: expected %q, got %q", "***", result)
	}
	for _, line := range []string{"line-one", "line-two"} {
		if result := mask.Mask(line); result != "***" {
			t.Errorf("secret line %q: expected %q, got %q", line, "***", result)
		}
	}
}
