package repoident

import (
	"cmp"
	"errors"
	"fmt"
	"net/url"
	"strings"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/samber/lo"
)

const (
	KnotServiceID         = "tangled_knot"
	KnotServiceType       = "TangledKnot"
	LegacyKnotServiceID   = "atproto_pds"
	LegacyKnotServiceType = "AtprotoPersonalDataServer"
)

type SchemePolicy int

const (
	RequireHTTPS SchemePolicy = iota
	AllowHTTP
)

func SchemeFor(allowHTTP bool) SchemePolicy {
	return lo.Ternary(allowHTTP, AllowHTTP, RequireHTTPS)
}

var (
	ErrNilIdentity   = errors.New("nil identity has no knot service endpoint")
	ErrNoKnotService = errors.New("DID document declares no " + KnotServiceID + " or " + LegacyKnotServiceID + " service endpoint")
	ErrZeroKnotURL   = errors.New("zero KnotURL has no base URL to encode")
)

var defaultPorts = map[string]string{"https": "443", "http": "80"}

type KnotURL struct {
	scheme string
	host   string
}

func (k KnotURL) IsZero() bool { return k.host == "" }

func (k KnotURL) Host() string { return k.host }

func (k KnotURL) url() *url.URL { return &url.URL{Scheme: k.scheme, Host: k.host} }

func (k KnotURL) String() string {
	if k.IsZero() {
		return ""
	}
	return k.url().String()
}

func (k KnotURL) JoinPath(elem ...string) string {
	return k.url().JoinPath(elem...).String()
}

func (k KnotURL) MarshalText() ([]byte, error) {
	if k.IsZero() {
		return nil, ErrZeroKnotURL
	}
	return []byte(k.String()), nil
}

func (k *KnotURL) UnmarshalText(text []byte) error {
	parsed, err := ParseKnotURL(string(text), AllowHTTP)
	if err != nil {
		return err
	}
	*k = parsed
	return nil
}

type pathMode int

const (
	rejectPath pathMode = iota
	stripPath
)

func ParseKnotURL(raw string, policy SchemePolicy) (KnotURL, error) {
	return parseBase(raw, policy, rejectPath)
}

func KnotURLFromIdentity(ident *identity.Identity, policy SchemePolicy) (KnotURL, error) {
	if ident == nil {
		return KnotURL{}, ErrNilIdentity
	}
	raw := cmp.Or(
		typedEndpoint(ident, KnotServiceID, KnotServiceType),
		typedEndpoint(ident, LegacyKnotServiceID, LegacyKnotServiceType),
	)
	if raw == "" {
		return KnotURL{}, ErrNoKnotService
	}
	return parseBase(raw, policy, stripPath)
}

func typedEndpoint(ident *identity.Identity, id, serviceType string) string {
	service := ident.Services[id]
	return lo.Ternary(service.Type == serviceType, service.URL, "")
}

func parseBase(raw string, policy SchemePolicy, paths pathMode) (KnotURL, error) {
	if raw == "" {
		return KnotURL{}, errors.New("empty knot URL")
	}
	u, err := url.Parse(raw)
	if err != nil {
		return KnotURL{}, fmt.Errorf("invalid knot URL %q: %w", raw, err)
	}
	if u.Hostname() == "" {
		return KnotURL{}, fmt.Errorf("knot URL %q has no host", raw)
	}
	if u.User != nil {
		return KnotURL{}, fmt.Errorf("knot URL %q has userinfo", raw)
	}
	if u.RawQuery != "" || u.Fragment != "" {
		return KnotURL{}, fmt.Errorf("knot URL %q has a query or fragment", raw)
	}
	if paths == rejectPath && u.Path != "" && u.Path != "/" {
		return KnotURL{}, fmt.Errorf("knot URL %q has a path", raw)
	}
	switch u.Scheme {
	case "https":
	case "http":
		if policy != AllowHTTP {
			return KnotURL{}, fmt.Errorf("knot URL %q must use https", raw)
		}
	default:
		return KnotURL{}, fmt.Errorf("knot URL %q has unsupported scheme %q", raw, u.Scheme)
	}
	return KnotURL{scheme: u.Scheme, host: canonicalHost(u)}, nil
}

func canonicalHost(u *url.URL) string {
	host := strings.ToLower(u.Host)
	switch port := u.Port(); port {
	case "":
		return strings.TrimSuffix(host, ":")
	case defaultPorts[u.Scheme]:
		return strings.TrimSuffix(host, ":"+port)
	default:
		return host
	}
}
