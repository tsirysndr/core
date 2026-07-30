package repoident

import (
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type RepoDid syntax.DID

func (r RepoDid) String() string { return string(r) }

func NewRepoDid(s string) (RepoDid, error) {
	did, err := syntax.ParseDID(s)
	if err != nil {
		return "", fmt.Errorf("invalid repoDid %q: %w", s, err)
	}
	return RepoDid(did), nil
}

func (r *RepoDid) UnmarshalText(text []byte) error {
	did, err := NewRepoDid(string(text))
	if err != nil {
		return err
	}
	*r = did
	return nil
}

type OwnerDid syntax.DID

func (o OwnerDid) String() string { return string(o) }

func NewOwnerDid(s string) (OwnerDid, error) {
	did, err := syntax.ParseDID(s)
	if err != nil {
		return "", fmt.Errorf("invalid ownerDid %q: %w", s, err)
	}
	return OwnerDid(did), nil
}
