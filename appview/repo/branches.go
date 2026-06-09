package repo

import (
	"encoding/json"
	"fmt"
	"net/http"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	xrpcclient "tangled.org/core/appview/xrpcclient"
	"tangled.org/core/types"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

func (rp *Repo) Branches(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoBranches")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	xrpcc := &indigoxrpc.Client{Host: rp.config.KnotMirror.Url}

	xrpcBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, f.RepoDid)
	if err != nil {
		l.Error("failed to call XRPC repo.branches", "err", err)
		rp.pages.Error503(w)
		return
	}
	var result types.RepoBranchesResponse
	if err := json.Unmarshal(xrpcBytes, &result); err != nil {
		l.Error("failed to decode XRPC response", "err", err)
		rp.pages.Error503(w)
		return
	}
	sortBranches(result.Branches)
	user := rp.oauth.GetMultiAccountUser(r)
	rp.pages.RepoBranches(w, pages.RepoBranchesParams{
		BaseParams:           pages.BaseParamsFromContext(r.Context()),
		RepoInfo:             rp.repoResolver.GetRepoInfo(r, user),
		RepoBranchesResponse: result,
	})
}

func (rp *Repo) DeleteBranch(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DeleteBranch")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	noticeId := "delete-branch-error"
	branch := r.FormValue("branch")
	if branch == "" {
		rp.pages.Notice(w, noticeId, "No branch provided.")
		return
	}
	client, err := rp.oauth.ServiceClient(
		r,
		oauth.WithService(f.Knot),
		oauth.WithLxm(tangled.RepoDeleteBranchNSID),
		oauth.WithDev(rp.config.Core.Dev),
	)
	if err != nil {
		l.Warn("Failed to connect to knotserver", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to connect to knotserver")
		return
	}
	err = tangled.RepoDeleteBranch(
		r.Context(),
		client,
		&tangled.RepoDeleteBranch_Input{
			Branch: branch,
			Repo:   f.RepoAt().String(),
		},
	)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("Failed to delete branch", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Notice(w, noticeId, fmt.Sprintf("Failed to delete branch: %s", xrpcerr))
		return
	}
	l.Error("deleted branch from knot", "branch", branch, "repo", f.RepoAt())
	rp.pages.HxRefresh(w)
}
