package xrpc

import (
	"net/http"
	"strings"

	"golang.org/x/crypto/ssh"
	"tangled.org/core/api/tangled"
	"tangled.org/core/rbac"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) CheckPushAllowed(w http.ResponseWriter, r *http.Request) {
	repo := strings.TrimSpace(r.URL.Query().Get("repo"))
	keyStr := r.URL.Query().Get("key")

	if !strings.HasPrefix(repo, "did:") || keyStr == "" {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("repo (a repo DID) and key are required"),
		), http.StatusBadRequest)
		return
	}

	offered, _, _, _, err := ssh.ParseAuthorizedKey([]byte(keyStr))
	if err != nil {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage("malformed public key"),
		), http.StatusBadRequest)
		return
	}

	did, ok, err := x.Db.DidForPublicKey(offered)
	if err != nil {
		x.Logger.Error("failed to look up public key", "error", err)
		x.writeJson(w, tangled.RepoCheckPushAllowed_Output{Allowed: false})
		return
	}
	if !ok {
		// unknown key, not an error, just not allowed
		x.writeJson(w, tangled.RepoCheckPushAllowed_Output{Allowed: false})
		return
	}

	didStr := did.String()

	allowed, err := x.Enforcer.IsPushAllowed(didStr, rbac.ThisServer, repo)
	if err != nil {
		x.Logger.Error("enforcer error", "did", didStr, "repo", repo, "error", err)
		allowed = false
	}

	out := tangled.RepoCheckPushAllowed_Output{Allowed: allowed}
	out.Did = &didStr
	x.writeJson(w, out)
}
