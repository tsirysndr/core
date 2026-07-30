package repoident

import (
	"encoding/json"
	"testing"
)

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

func TestDidJSONRoundTripsAndValidates(t *testing.T) {
	var pair struct {
		Repo  RepoDid  `json:"repo"`
		Owner OwnerDid `json:"owner"`
	}
	const raw = `{"repo":"did:plc:boltless","owner":"did:plc:akshay"}`
	if err := json.Unmarshal([]byte(raw), &pair); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	out, err := json.Marshal(pair)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if string(out) != raw {
		t.Errorf("re-encoded %s, want %s", out, raw)
	}
	if err := json.Unmarshal([]byte(`{"repo":"not-a-did","owner":"did:plc:akshay"}`), &pair); err == nil {
		t.Error("Unmarshal accepted a malformed repoDid")
	}
}
