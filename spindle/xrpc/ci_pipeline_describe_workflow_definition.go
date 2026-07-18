package xrpc

import (
	"net/http"

	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) DescribeWorkflowDefinition(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, http.StatusBadRequest)
	}

	q := r.URL.Query()
	repoDid, xerr, ok := x.resolveKnownRepoDid(q.Get("repo"))
	if !ok {
		fail(xerr)
		return
	}

	sha := q.Get("sha")
	if err := requireSha(sha); err != nil {
		fail(xrpcerr.NewXrpcError(xrpcerr.WithTag("InvalidRequest"), xrpcerr.WithError(err)))
		return
	}

	sourceRepoParam := q.Get("sourceRepo")
	sourceRepo, err := parseOptionalDID("sourceRepo", &sourceRepoParam)
	if err != nil {
		fail(xrpcerr.NewXrpcError(xrpcerr.WithTag("InvalidRequest"), xrpcerr.WithError(err)))
		return
	}

	out, err := x.Trigger.DescribeWorkflowDefinition(r.Context(), repoDid, sha, sourceRepo)
	if err != nil {
		fail(xrpcerr.GenericError(err))
		return
	}

	if err := writeJson(w, http.StatusOK, out); err != nil {
		l.Error("failed to write response", "err", err)
	}
}
