package xrpc

import (
	"fmt"
	"net/http"

	"tangled.org/core/api/tangled"
	"tangled.org/core/gitutil"
	"tangled.org/core/knotserver/git"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) RepoArchive(w http.ResponseWriter, r *http.Request) {
	params, err := gitutil.ParseArchiveParams(r.URL.Query())
	if err != nil {
		writeError(w, xrpcerr.NewXrpcError(
			xrpcerr.WithTag("InvalidRequest"),
			xrpcerr.WithMessage(err.Error()),
		), http.StatusBadRequest)
		return
	}

	repo := r.URL.Query().Get("repo")
	resolved, err := x.resolveRepo(repo)
	if err != nil {
		writeError(w, err.(xrpcerr.XrpcError), http.StatusBadRequest)
		return
	}

	// ref can be empty (git.Open handles this)
	gr, err := git.Open(resolved.path, params.Rev.String())
	if err != nil {
		writeError(w, xrpcerr.RefNotFoundError, http.StatusNotFound)
		return
	}

	hash := gitutil.RevFromHash(gr.Hash())
	served := params.WithRev(params.Rev.Or(hash)).Serve(resolved.name).WithRev(hash)

	served.SetHeaders(w.Header())
	w.Header().Set("Link", gitutil.ImmutableLink(x.archiveURL(repo, served)))
	if served.ServeNotModified(w, r, gitutil.RepoIdentity(resolved.path)) {
		return
	}

	body := gitutil.NewResponseBody(w)
	if err := gitutil.WriteArchive(r.Context(), body, resolved.path, served); err != nil {
		x.Logger.Error("writing archive", "error", err.Error(), "format", params.Format)
		body.Fail()
	}
}

func (x *Xrpc) archiveURL(repo string, params gitutil.ServedArchive) string {
	scheme := "https"
	if x.Config.Server.Dev {
		scheme = "http"
	}
	return fmt.Sprintf("%s://%s/xrpc/%s?%s",
		scheme, x.Config.Server.Hostname, tangled.RepoArchiveNSID, params.Query(repo).Encode())
}
