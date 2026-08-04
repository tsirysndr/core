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

	hash := gr.Hash()
	params.Rev = params.Rev.OrHash(hash)
	params.Prefix = params.Prefix.OrDefault(resolved.name, params.Rev)

	params.SetHeaders(w.Header(), resolved.name)
	w.Header().Set("Link", gitutil.ImmutableLink(
		x.archiveURL(repo, params.WithRev(gitutil.RevFromHash(hash))),
	))

	if err := gitutil.WriteArchive(r.Context(), w, resolved.path, gitutil.RevFromHash(hash), params.Format, params.Prefix); err != nil {
		// once we start writing to the body we can't report error anymore
		// so we are only left with logging the error
		x.Logger.Error("writing archive", "error", err.Error(), "format", params.Format)
	}
}

func (x *Xrpc) archiveURL(repo string, params gitutil.ArchiveParams) string {
	scheme := "https"
	if x.Config.Server.Dev {
		scheme = "http"
	}
	return fmt.Sprintf("%s://%s/xrpc/%s?%s",
		scheme, x.Config.Server.Hostname, tangled.RepoArchiveNSID, params.Query(repo).Encode())
}
