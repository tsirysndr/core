package repoverify

import "testing"

func TestNewRepoDid_RejectsInvalid(t *testing.T) {
	if _, err := NewRepoDid(""); err == nil {
		t.Error("NewRepoDid(\"\") err = nil, want error")
	}
}

func TestNewRepoDid_AcceptsValid(t *testing.T) {
	raw := "did:plc:abc123abc123abc123abc123"
	got, err := NewRepoDid(raw)
	if err != nil {
		t.Fatalf("NewRepoDid(%q): %v", raw, err)
	}
	if got.String() != raw {
		t.Errorf("got %q, want %q", got, raw)
	}
}

func TestParseKnotEndpoint_RejectsHttpInProd(t *testing.T) {
	if _, err := ParseKnotEndpoint("http://knot.example", false); err == nil {
		t.Error("http:// knot URL accepted in prod")
	}
}

func TestParseKnotEndpoint_AllowsHttpInDev(t *testing.T) {
	u, err := ParseKnotEndpoint("http://knot.example", true)
	if err != nil {
		t.Fatalf("dev mode should allow http: %v", err)
	}
	if u.Host != "knot.example" {
		t.Errorf("Host = %q, want knot.example", u.Host)
	}
}

func TestParseKnotEndpoint_RejectsUnsupportedScheme(t *testing.T) {
	if _, err := ParseKnotEndpoint("ftp://knot.example", true); err == nil {
		t.Error("ParseKnotEndpoint accepted ftp:// in dev")
	}
	if _, err := ParseKnotEndpoint("ftp://knot.example", false); err == nil {
		t.Error("ParseKnotEndpoint accepted ftp:// in prod")
	}
}

func TestParseKnotEndpoint_RejectsEmptyOrHostless(t *testing.T) {
	cases := []string{"", "https://", "not a url at all"}
	for _, raw := range cases {
		t.Run(raw, func(t *testing.T) {
			if _, err := ParseKnotEndpoint(raw, false); err == nil {
				t.Errorf("ParseKnotEndpoint(%q) accepted bogus URL", raw)
			}
		})
	}
}

func TestParseKnotEndpoint_HostPreservesPort(t *testing.T) {
	u, err := ParseKnotEndpoint("http://localhost:3000", true)
	if err != nil {
		t.Fatalf("ParseKnotEndpoint: %v", err)
	}
	if u.Host != "localhost:3000" {
		t.Errorf("Host = %q, want localhost:3000", u.Host)
	}
}
