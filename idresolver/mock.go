package idresolver

import (
	"context"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
)

func NewMockResolver(dir identity.Directory) *Resolver {
	return &Resolver{
		directory: dir,
		base:      &identity.BaseDirectory{},
	}
}

type MockDirectory struct {
	Ident *identity.Identity
}

func (m MockDirectory) LookupDID(context.Context, syntax.DID) (*identity.Identity, error) {
	return m.Ident, nil
}

func (m MockDirectory) LookupHandle(context.Context, syntax.Handle) (*identity.Identity, error) {
	return m.Ident, nil
}

func (m MockDirectory) Lookup(context.Context, syntax.AtIdentifier) (*identity.Identity, error) {
	return m.Ident, nil
}

func (m MockDirectory) Purge(context.Context, syntax.AtIdentifier) error {
	return nil
}
