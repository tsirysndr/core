package eventconsumer

import (
	"net/url"
	"strconv"

	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/hostutil"
)

type Kind string

const (
	KindKnot    Kind = "knot"
	KindSpindle Kind = "spindle"
)

type Source struct {
	Kind  Kind
	Host  string
	NoTLS bool // use TLS by default
}

func NewKnotSource(host string) Source {
	host, noTLS, _ := hostutil.ParseHostname(host)
	return Source{Kind: KindKnot, Host: host, NoTLS: noTLS}
}
func NewSpindleSource(host string) Source {
	host, noTLS, _ := hostutil.ParseHostname(host)
	return Source{Kind: KindSpindle, Host: host, NoTLS: noTLS}
}

func (s Source) Key() string { return string(s.Kind) + ":" + s.Host }

func MigrateLegacyCursor(store cursor.Store, s Source) {
	if store.Get(s.Key()) != 0 {
		return
	}
	if legacy := store.Get(s.Host); legacy != 0 {
		store.Set(s.Key(), legacy)
	}
}

func (s Source) URL(cursor int64) (*url.URL, error) {
	scheme := "wss"
	if s.NoTLS {
		scheme = "ws"
	}
	u, err := url.Parse(scheme + "://" + s.Host + "/events")
	if err != nil {
		return nil, err
	}
	if cursor != 0 {
		q := url.Values{}
		q.Add("cursor", strconv.FormatInt(cursor, 10))
		u.RawQuery = q.Encode()
	}
	return u, nil
}
