package idresolver

import "github.com/bluesky-social/indigo/atproto/identity"

func NewMockResolver(dir identity.Directory) *Resolver {
	return &Resolver{
		directory: dir,
		base:      &identity.BaseDirectory{},
	}
}
