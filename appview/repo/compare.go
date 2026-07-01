package repo

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"regexp"
	"strings"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/pages"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"
	xrpcclient "tangled.org/core/xrpc/xrpcclient"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
)

var shaPattern = regexp.MustCompile(`^[0-9a-f]{4,40}$`)

func (rp *Repo) CompareNew(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoCompareNew")

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	xrpcc := &indigoxrpc.Client{Host: rp.config.KnotMirror.Url}

	branchBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, f.RepoDid)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.branches", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var branchResult types.RepoBranchesResponse
	if err := json.Unmarshal(branchBytes, &branchResult); err != nil {
		l.Error("failed to decode XRPC branches response", "err", err)
		rp.pages.Notice(w, "compare-error", "Failed to produce comparison. Try again later.")
		return
	}
	branches := branchResult.Branches

	sortBranches(branches)

	var defaultBranch string
	for _, b := range branches {
		if b.IsDefault {
			defaultBranch = b.Name
		}
	}

	base := defaultBranch
	head := defaultBranch

	params := r.URL.Query()
	queryBase := params.Get("base")
	queryHead := params.Get("head")
	if queryBase != "" {
		base = queryBase
	}
	if queryHead != "" {
		head = queryHead
	}

	tagBytes, err := tangled.GitTempListTags(r.Context(), xrpcc, "", 0, f.RepoDid)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.tags", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var tags types.RepoTagsResponse
	if err := json.Unmarshal(tagBytes, &tags); err != nil {
		l.Error("failed to decode XRPC tags response", "err", err)
		rp.pages.Notice(w, "compare-error", "Failed to produce comparison. Try again later.")
		return
	}

	rp.pages.RepoCompareNew(w, pages.RepoCompareNewParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Branches:   branches,
		Tags:       tags.Tags,
		Base:       base,
		Head:       head,
	})
}

func (rp *Repo) Compare(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoCompare")

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	var diffOpts types.DiffOpts
	if d := r.URL.Query().Get("diff"); d == "split" {
		diffOpts.Split = true
	}

	// if user is navigating to one of
	//   /compare/{base}...{head}
	//   /compare/{base}/{head}
	var base, head string
	rest := chi.URLParam(r, "*")

	var parts []string
	if strings.Contains(rest, "...") {
		parts = strings.SplitN(rest, "...", 2)
	} else if strings.Contains(rest, "/") {
		parts = strings.SplitN(rest, "/", 2)
	}

	if len(parts) == 2 {
		base = parts[0]
		head = parts[1]
	}

	base, _ = url.PathUnescape(base)
	head, _ = url.PathUnescape(head)

	if base == "" || head == "" {
		l.Error("invalid comparison")
		rp.pages.Error404(w)
		return
	}

	if shaPattern.MatchString(base) || shaPattern.MatchString(head) {
		http.Error(w, "comparing by commit SHA is not allowed, use a branch or tag name", http.StatusForbidden)
		return
	}

	scheme := "http"
	if !rp.config.Core.Dev {
		scheme = "https"
	}
	host := fmt.Sprintf("%s://%s", scheme, f.Knot)
	xrpcc := &indigoxrpc.Client{
		Host: host,
	}

	repoId := f.RepoIdentifier()

	branchBytes, err := tangled.RepoBranches(r.Context(), xrpcc, "", 0, repoId)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.branches", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var branches types.RepoBranchesResponse
	if err := json.Unmarshal(branchBytes, &branches); err != nil {
		l.Error("failed to decode XRPC branches response", "err", err)
		rp.pages.Notice(w, "compare-error", "Failed to produce comparison. Try again later.")
		return
	}

	tagBytes, err := tangled.RepoTags(r.Context(), xrpcc, "", 0, repoId)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.tags", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var tags types.RepoTagsResponse
	if err := json.Unmarshal(tagBytes, &tags); err != nil {
		l.Error("failed to decode XRPC tags response", "err", err)
		rp.pages.Notice(w, "compare-error", "Failed to produce comparison. Try again later.")
		return
	}

	compareBytes, err := tangled.RepoCompare(r.Context(), xrpcc, repoId, base, head)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.compare", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var formatPatch types.RepoFormatPatchResponse
	if err := json.Unmarshal(compareBytes, &formatPatch); err != nil {
		l.Error("failed to decode XRPC compare response", "err", err)
		rp.pages.Notice(w, "compare-error", "Failed to produce comparison. Try again later.")
		return
	}

	var diff types.NiceDiff
	if formatPatch.CombinedPatchRaw != "" {
		diff = patchutil.AsNiceDiff(formatPatch.CombinedPatchRaw, base)
	} else {
		diff = patchutil.AsNiceDiff(formatPatch.FormatPatchRaw, base)
	}

	rp.pages.RepoCompare(w, pages.RepoCompareParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Branches:   branches.Branches,
		Tags:       tags.Tags,
		Base:       base,
		Head:       head,
		Diff:       &diff,
		DiffOpts:   diffOpts,
	})

}
