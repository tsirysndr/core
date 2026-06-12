package eventconsumer

import (
	"net/url"
	"strconv"

	"tangled.org/core/eventconsumer/cursor"
)

type Kind string

const (
	KindKnot    Kind = "knot"
	KindSpindle Kind = "spindle"
)

type Source struct {
	Kind Kind
	Host string
}

func NewKnotSource(host string) Source    { return Source{Kind: KindKnot, Host: host} }
func NewSpindleSource(host string) Source { return Source{Kind: KindSpindle, Host: host} }

func (s Source) Key() string { return string(s.Kind) + ":" + s.Host }

func MigrateLegacyCursor(store cursor.Store, s Source) {
	if store.Get(s.Key()) != 0 {
		return
	}
	if legacy := store.Get(s.Host); legacy != 0 {
		store.Set(s.Key(), legacy)
	}
}

func DefaultURL(dev bool) func(Source, int64) (*url.URL, error) {
	scheme := "wss"
	if dev {
		scheme = "ws"
	}
	return func(s Source, cursor int64) (*url.URL, error) {
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
}
