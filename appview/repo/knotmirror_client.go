package repo

import (
	"github.com/bluesky-social/indigo/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

// newKnotMirrorXRPCClient is created once per Repo service. A nil
// indigoxrpc.Client creates a new RobustHTTPClient for each XRPC call, which
// also creates a new transport and defeats connection reuse.
func newKnotMirrorXRPCClient(host string) *indigoxrpc.Client {
	return &indigoxrpc.Client{
		Host:   host,
		Client: util.RobustHTTPClient(),
	}
}

func (rp *Repo) knotMirrorXRPCClient() *indigoxrpc.Client {
	return rp.knotMirrorXRPC
}
