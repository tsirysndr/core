package repoident

import "testing"

func TestNewRepoDid_RejectsInvalid(t *testing.T) {
	if _, err := NewRepoDid(""); err == nil {
		t.Error("NewRepoDid(\"\") err = nil, want error")
	}
}

func TestNewRepoDid_AcceptsValid(t *testing.T) {
	raw := "did:plc:boltless"
	got, err := NewRepoDid(raw)
	if err != nil {
		t.Fatalf("NewRepoDid(%q): %v", raw, err)
	}
	if got.String() != raw {
		t.Errorf("got %q, want %q", got, raw)
	}
}

func TestNewOwnerDid_RejectsInvalid(t *testing.T) {
	if _, err := NewOwnerDid("not-a-did"); err == nil {
		t.Error("NewOwnerDid(\"not-a-did\") err = nil, want error")
	}
}

func TestNewOwnerDid_AcceptsValid(t *testing.T) {
	raw := "did:plc:akshay"
	got, err := NewOwnerDid(raw)
	if err != nil {
		t.Fatalf("NewOwnerDid(%q): %v", raw, err)
	}
	if got.String() != raw {
		t.Errorf("got %q, want %q", got, raw)
	}
}
