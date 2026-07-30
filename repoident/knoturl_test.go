package repoident

import (
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/bluesky-social/indigo/atproto/identity"
)

func knotService(url string) map[string]identity.ServiceEndpoint {
	return map[string]identity.ServiceEndpoint{KnotServiceID: {Type: KnotServiceType, URL: url}}
}

func identWith(services map[string]identity.ServiceEndpoint) *identity.Identity {
	return &identity.Identity{Services: services}
}

func mustParse(t *testing.T, raw string) KnotURL {
	t.Helper()
	u, err := ParseKnotURL(raw, AllowHTTP)
	if err != nil {
		t.Fatalf("ParseKnotURL(%q): %v", raw, err)
	}
	return u
}

func TestKnotURL_ParsedAsBaseAndAsServiceEndpoint(t *testing.T) {
	const canonical = "https://knot.oyster.cafe"
	const rejected = ""
	cases := map[string]struct{ wantAsBase, wantAsEndpoint string }{
		canonical:                              {canonical, canonical},
		canonical + "/":                        {canonical, canonical},
		"HTTPS://Knot.Oyster.Cafe/":            {canonical, canonical},
		"https://KNOT.OYSTER.CAFE:443":         {canonical, canonical},
		"https://knot.oyster.cafe:":            {canonical, canonical},
		"https://knot.oyster.cafe:80":          {canonical + ":80", canonical + ":80"},
		"https://[2001:DB8::1]:443":            {"https://[2001:db8::1]", "https://[2001:db8::1]"},
		"https://[2001:db8::1]:8443":           {"https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"},
		"https://[fe80::1%25eth0]":             {"https://[fe80::1%25eth0]", "https://[fe80::1%25eth0]"},
		"https://☃.oyster.cafe":                {"https://%E2%98%83.oyster.cafe", "https://%E2%98%83.oyster.cafe"},
		canonical + "/repo/m5326fp3qemiriiqy":  {rejected, canonical},
		canonical + "/base":                    {rejected, canonical},
		canonical + "?utm=knot":                {rejected, rejected},
		canonical + "#pulls":                   {rejected, rejected},
		"https://nel@knot.oyster.cafe":         {rejected, rejected},
		"https://nel:hunter2@knot.oyster.cafe": {rejected, rejected},
		"ftp://knot.oyster.cafe":               {rejected, rejected},
		"http://knot.oyster.cafe":              {rejected, rejected},
		"knot.oyster.cafe":                     {rejected, rejected},
		"https://":                             {rejected, rejected},
		"https://:443":                         {rejected, rejected},
		"not a url at all":                     {rejected, rejected},
		"":                                     {rejected, rejected},
	}
	for raw, want := range cases {
		t.Run(raw, func(t *testing.T) {
			check := func(door string, got KnotURL, err error, want string) {
				switch {
				case want == rejected:
					if err == nil {
						t.Errorf("%s accepted %q as %q, want an error", door, raw, got)
					}
				case err != nil:
					t.Errorf("%s(%q): %v", door, raw, err)
				case got.String() != want:
					t.Errorf("%s(%q) = %q, want %q", door, raw, got, want)
				case got != mustParse(t, want):
					t.Errorf("%s(%q) doesn't compare equal to its canonical spelling %q", door, raw, want)
				}
			}
			base, baseErr := ParseKnotURL(raw, RequireHTTPS)
			check("ParseKnotURL", base, baseErr, want.wantAsBase)
			endpoint, endpointErr := KnotURLFromIdentity(identWith(knotService(raw)), RequireHTTPS)
			check("KnotURLFromIdentity", endpoint, endpointErr, want.wantAsEndpoint)
		})
	}
}

func TestKnotURLFromIdentity_PicksTheKnotService(t *testing.T) {
	const legacyURL = "https://knot.oyster.cafe"
	const tangledURL = "https://nel.pet/repo/fcicrjbr6oh3"
	cases := map[string]struct{ tangledType, want string }{
		"legacy atproto_pds is the fallback":          {"", legacyURL},
		"tangled_knot wins over legacy":               {KnotServiceType, "https://nel.pet"},
		"tangled_knot with the wrong type is ignored": {"AtprotoLabeler", legacyURL},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			services := map[string]identity.ServiceEndpoint{
				LegacyKnotServiceID: {Type: LegacyKnotServiceType, URL: legacyURL},
			}
			if tc.tangledType != "" {
				services[KnotServiceID] = identity.ServiceEndpoint{Type: tc.tangledType, URL: tangledURL}
			}
			u, err := KnotURLFromIdentity(identWith(services), RequireHTTPS)
			if err != nil {
				t.Fatalf("KnotURLFromIdentity: %v", err)
			}
			if u.String() != tc.want {
				t.Errorf("KnotURLFromIdentity = %q, want %q", u, tc.want)
			}
		})
	}
}

func TestKnotURLFromIdentity_ErrNoKnotService(t *testing.T) {
	cases := map[string]*identity.Identity{
		"nothing declared": identWith(nil),
		"another service only": identWith(map[string]identity.ServiceEndpoint{
			"atproto_labeler": {Type: "AtprotoLabeler", URL: "https://nel.pet"},
		}),
		"tangled_knot with an empty url": identWith(knotService("")),
		"legacy service with the wrong type": identWith(map[string]identity.ServiceEndpoint{
			LegacyKnotServiceID: {Type: "AtprotoLabeler", URL: "https://knot.oyster.cafe"},
		}),
	}
	for name, ident := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := KnotURLFromIdentity(ident, RequireHTTPS); !errors.Is(err, ErrNoKnotService) {
				t.Errorf("error = %v, want ErrNoKnotService", err)
			}
		})
	}
	if _, err := KnotURLFromIdentity(nil, RequireHTTPS); !errors.Is(err, ErrNilIdentity) {
		t.Errorf("nil identity error = %v, want ErrNilIdentity", err)
	}
}

func TestKnotURL_ErrorQuotesTheDeclaredEndpoint(t *testing.T) {
	const declared = "https://knot.oyster.cafe/repo/limpet?utm=knot"
	_, err := KnotURLFromIdentity(identWith(knotService(declared)), RequireHTTPS)
	if err == nil {
		t.Fatal("KnotURLFromIdentity accepted a query")
	}
	if !strings.Contains(err.Error(), declared) {
		t.Errorf("error %q doesn't quote the declared endpoint %q", err, declared)
	}
}

func TestSchemePolicy_OnlyAllowHTTPPermitsPlaintext(t *testing.T) {
	if RequireHTTPS != 0 || SchemeFor(true) != AllowHTTP || SchemeFor(false) != RequireHTTPS {
		t.Fatalf("RequireHTTPS=%d SchemeFor(true)=%d: the zero value must stay RequireHTTPS", RequireHTTPS, SchemeFor(true))
	}
	if u := mustParse(t, "http://knot.oyster.cafe:80"); u.String() != "http://knot.oyster.cafe" {
		t.Errorf("AllowHTTP parse = %q, want http://knot.oyster.cafe", u)
	}
	for _, policy := range []SchemePolicy{RequireHTTPS, SchemePolicy(42), SchemePolicy(-1)} {
		if _, err := ParseKnotURL("http://knot.oyster.cafe", policy); err == nil {
			t.Errorf("policy %d permitted http", policy)
		}
	}
}

func TestKnotURL_JoinPathKeepsTheDidColons(t *testing.T) {
	const want = "http://localhost:5555/did:plc:limpet"
	if got := mustParse(t, "http://localhost:5555").JoinPath("did:plc:limpet"); got != want {
		t.Errorf("JoinPath = %q, want %q", got, want)
	}
}

func TestKnotURL_ZeroValueIsInert(t *testing.T) {
	var zero KnotURL
	if !zero.IsZero() || zero.String() != "" || zero.Host() != "" {
		t.Errorf("zero KnotURL isn't inert: IsZero=%v String=%q Host=%q", zero.IsZero(), zero.String(), zero.Host())
	}
	if _, err := zero.MarshalText(); !errors.Is(err, ErrZeroKnotURL) {
		t.Errorf("zero KnotURL MarshalText error = %v, want ErrZeroKnotURL", err)
	}
}

func TestKnotURL_JSONCanonicalizesAndValidates(t *testing.T) {
	var decoded struct {
		Knot KnotURL `json:"knot"`
	}
	if err := json.Unmarshal([]byte(`{"knot":"HTTP://Localhost:80/"}`), &decoded); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if decoded.Knot.Host() != "localhost" {
		t.Errorf("decoded host = %q, want localhost", decoded.Knot.Host())
	}
	out, err := json.Marshal(decoded)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if string(out) != `{"knot":"http://localhost"}` {
		t.Errorf("re-encoded = %s, want {\"knot\":\"http://localhost\"}", out)
	}
	if err := json.Unmarshal([]byte(`{"knot":"https://knot.oyster.cafe/repo/limpet"}`), &decoded); err == nil {
		t.Error("Unmarshal accepted a URL with a path")
	}
}
