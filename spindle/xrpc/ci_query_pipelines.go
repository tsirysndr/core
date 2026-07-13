package xrpc

import (
	"fmt"
	"net/http"
	"strconv"

	"tangled.org/core/api/tangled"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) HandleCiQueryPipelines(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	repo := r.URL.Query().Get("repo")
	if repo == "" {
		fail(xrpcerr.GenericError(fmt.Errorf("missing repo parameter")), http.StatusBadRequest)
		return
	}

	commits := r.URL.Query()["commits"]
	cursor := r.URL.Query().Get("cursor")
	kinds := r.URL.Query()["kinds"]
	limitStr := r.URL.Query().Get("limit")

	limit := 30
	if limitStr != "" {
		if val, err := strconv.Atoi(limitStr); err == nil && val > 0 {
			limit = val
		}
	}

	pipelines, nextCursor, total, err := x.Db.QueryPipelines(r.Context(), repo, commits, cursor, kinds, limit)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	output := tangled.CiQueryPipelines_Output{
		Pipelines: pipelines,
		Total:     total,
	}
	if nextCursor != "" {
		output.Cursor = &nextCursor
	}

	if err := writeJson(w, http.StatusOK, output); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
	}
}

func (x *Xrpc) HandleCiGetPipeline(w http.ResponseWriter, r *http.Request) {
	l := x.Logger
	fail := func(e xrpcerr.XrpcError, status int) {
		l.Error("failed", "kind", e.Tag, "error", e.Message)
		writeError(w, e, status)
	}

	pipeline := r.URL.Query().Get("pipeline")
	if pipeline == "" {
		fail(xrpcerr.GenericError(fmt.Errorf("missing pipeline parameter")), http.StatusBadRequest)
		return
	}

	p, err := x.Db.GetPipeline(r.Context(), pipeline)
	if err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
		return
	}

	if err := writeJson(w, http.StatusOK, p); err != nil {
		fail(xrpcerr.GenericError(err), http.StatusInternalServerError)
	}
}
