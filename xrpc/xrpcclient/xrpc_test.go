package xrpcclient

import (
	"errors"
	"fmt"
	"testing"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

func TestA404IsARouteAnswerOnlyWithAnErrorName(t *testing.T) {
	cases := map[string]struct {
		wrapped  error
		wantName string
		want     error
	}{
		"an error name means the knot served the route": {
			&indigoxrpc.XRPCError{ErrStr: "RepoNotFound"}, "RepoNotFound", ErrXrpcNotFound,
		},
		"a 404 whose body won't decode means the route is missing": {
			fmt.Errorf("failed to decode xrpc error message: unexpected end of JSON input"), "", ErrXrpcUnsupported,
		},
		"a 404 body without an error name can't claim the route": {
			&indigoxrpc.XRPCError{Message: "not found"}, "", ErrXrpcUnsupported,
		},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			err := &indigoxrpc.Error{StatusCode: 404, Wrapped: tc.wrapped}
			if got := ErrorName(err); got != tc.wantName {
				t.Errorf("ErrorName = %q, want %q", got, tc.wantName)
			}
			if got := HandleXrpcErr(err); !errors.Is(got, tc.want) {
				t.Errorf("HandleXrpcErr = %v, want %v", got, tc.want)
			}
		})
	}
	for _, err := range []error{errors.New("connection refused"), nil} {
		if got := ErrorName(err); got != "" {
			t.Errorf("ErrorName(%v) = %q, want empty, since only a lexicon error has an error name", err, got)
		}
	}
}
