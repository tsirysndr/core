package serviceauth

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"path"
	"strings"

	"github.com/bluesky-social/indigo/atproto/auth"
	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/log"
	xrpcerr "tangled.org/core/xrpc/errors"
)

type contextKey string

const ActorDid contextKey = "ActorDid"

func DidWeb(hostname string) syntax.DID {
	return syntax.DID("did:web:" + strings.ReplaceAll(hostname, ":", "%3A"))
}

type ServiceAuth struct {
	logger      *slog.Logger
	dir         identity.Directory
	audienceDid string
}

func NewServiceAuth(logger *slog.Logger, dir identity.Directory, audienceDid string) *ServiceAuth {
	return &ServiceAuth{
		logger:      log.SubLogger(logger, "serviceauth"),
		dir:         dir,
		audienceDid: audienceDid,
	}
}

func (sa *ServiceAuth) VerifyServiceAuth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		token := r.Header.Get("Authorization")
		token = strings.TrimPrefix(token, "Bearer ")

		lxm, err := syntax.ParseNSID(path.Base(r.URL.Path))
		if err != nil {
			sa.logger.Error("could not derive lexicon method from request path", "path", r.URL.Path, "err", err)
			writeError(w, xrpcerr.AuthError(err), http.StatusForbidden)
			return
		}

		s := auth.ServiceAuthValidator{
			Audience: sa.audienceDid,
			Dir:      sa.dir,
		}

		did, err := s.Validate(r.Context(), token, &lxm)
		if err != nil {
			sa.logger.Error("signature verification failed", "err", err)
			writeError(w, xrpcerr.AuthError(err), http.StatusForbidden)
			return
		}

		sa.logger.Debug("valid signature", "did", did)

		r = r.WithContext(
			context.WithValue(r.Context(), ActorDid, did),
		)

		next.ServeHTTP(w, r)
	})
}

// this is slightly different from http_util::write_error to follow the spec:
//
// the json object returned must include an "error" and a "message"
func writeError(w http.ResponseWriter, e xrpcerr.XrpcError, status int) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(e)
}
