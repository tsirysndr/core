package xrpcclient

import (
	"errors"
	"net/http"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

var (
	ErrXrpcUnsupported  = errors.New("xrpc not supported on this knot")
	ErrXrpcNotFound     = errors.New("not found on this knot")
	ErrXrpcUnauthorized = errors.New("unauthorized xrpc request")
	ErrXrpcForbidden    = errors.New("forbidden xrpc request")
	ErrXrpcFailed       = errors.New("xrpc request failed")
	ErrXrpcInvalid      = errors.New("invalid xrpc request")
)

// produces a more manageable error
func HandleXrpcErr(err error) error {
	if err == nil {
		return nil
	}

	var xrpcerr *indigoxrpc.Error
	if ok := errors.As(err, &xrpcerr); !ok {
		return ErrXrpcInvalid
	}

	switch xrpcerr.StatusCode {
	case http.StatusNotFound:
		if ErrorName(err) != "" {
			return ErrXrpcNotFound
		}
		return ErrXrpcUnsupported
	case http.StatusUnauthorized:
		return ErrXrpcUnauthorized
	case http.StatusForbidden:
		return ErrXrpcForbidden
	default:
		return ErrXrpcFailed
	}
}

func ErrorName(err error) string {
	var named *indigoxrpc.XRPCError
	if !errors.As(err, &named) {
		return ""
	}
	return named.ErrStr
}
