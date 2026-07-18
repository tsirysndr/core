package repo

import (
	"fmt"
	"net/http"
	"net/url"
	"path"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/types"
	xrpcclient "tangled.org/core/xrpc/xrpcclient"

	"github.com/go-chi/chi/v5"
	"github.com/go-git/go-git/v5/plumbing"
)

func (rp *Repo) Tree(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoTree")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to fully resolve repo", "err", err)
		return
	}
	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)
	// if the tree path has a trailing slash, let's strip it
	// so we don't 404
	treePath := chi.URLParam(r, "*")
	treePath, _ = url.PathUnescape(treePath)
	treePath = strings.TrimSuffix(treePath, "/")

	xrpcc := rp.knotMirrorXRPCClient()
	xrpcResp, err := tangled.GitTempGetTree(r.Context(), xrpcc, treePath, ref, f.RepoDid)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.tree", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	// redirects tree paths trying to access a blob; in this case the result.Files is unpopulated,
	// so we can safely redirect to the "parent" (which is the same file).
	if len(xrpcResp.Files) == 0 && xrpcResp.Parent != nil && *xrpcResp.Parent == treePath {
		redirectTo := fmt.Sprintf("/%s/blob/%s/%s", ownerSlashRepo, url.PathEscape(ref), *xrpcResp.Parent)
		http.Redirect(w, r, redirectTo, http.StatusFound)
		return
	}

	var readmeFile *tangled.GitTempGetTree_TreeEntry
	// Convert XRPC response to internal types.RepoTreeResponse
	files := make([]types.NiceTree, len(xrpcResp.Files))
	for i, xrpcFile := range xrpcResp.Files {
		file := types.NiceTree{
			Name: xrpcFile.Name,
			Mode: xrpcFile.Mode,
			Size: int64(xrpcFile.Size),
		}
		// Convert last commit info if present
		if xrpcFile.Last_commit != nil {
			commitWhen, _ := time.Parse(time.RFC3339, xrpcFile.Last_commit.When)
			file.LastCommit = &types.LastCommitInfo{
				Hash:    plumbing.NewHash(xrpcFile.Last_commit.Hash),
				Message: xrpcFile.Last_commit.Message,
				When:    commitWhen,
			}
		}
		files[i] = file
		if markup.IsReadmeFile(xrpcFile.Name, xrpcFile.Mode) {
			readmeFile = xrpcFile
		}
	}
	sortFiles(files)

	var (
		readmeFileName    string
		readmeFileContent string
	)
	if readmeFile != nil {
		bytes, err := tangled.GitTempGetBlob(r.Context(), xrpcc, path.Join(treePath, readmeFile.Name), ref, f.RepoDid)
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC git.getBlob", "xrpcerr", xrpcerr, "err", err)
			rp.pages.Error503(w)
			return
		}
		readmeFileName = readmeFile.Name
		readmeFileContent = string(bytes)
	}
	var breadcrumbs [][]string
	breadcrumbs = append(breadcrumbs, []string{f.Name, fmt.Sprintf("/%s/tree/%s", ownerSlashRepo, url.PathEscape(ref))})
	if treePath != "" {
		for idx, elem := range strings.Split(treePath, "/") {
			breadcrumbs = append(breadcrumbs, []string{elem, fmt.Sprintf("%s/%s", breadcrumbs[idx][1], url.PathEscape(elem))})
		}
	}

	// Get email to DID mapping for commit author
	var emails []string
	if xrpcResp.LastCommit != nil && xrpcResp.LastCommit.Author != nil {
		emails = append(emails, xrpcResp.LastCommit.Author.Email)
	}
	emailToDidMap, err := db.GetEmailToDid(rp.db, emails, true)
	if err != nil {
		l.Error("failed to get email to did mapping", "err", err)
		emailToDidMap = make(map[string]string)
	}

	var lastCommitInfo *types.LastCommitInfo
	if xrpcResp.LastCommit != nil {
		when, _ := time.Parse(time.RFC3339, xrpcResp.LastCommit.When)
		lastCommitInfo = &types.LastCommitInfo{
			Hash:    plumbing.NewHash(xrpcResp.LastCommit.Hash),
			Message: xrpcResp.LastCommit.Message,
			When:    when,
		}
		if xrpcResp.LastCommit.Author != nil {
			lastCommitInfo.Author.Name = xrpcResp.LastCommit.Author.Name
			lastCommitInfo.Author.Email = xrpcResp.LastCommit.Author.Email
			lastCommitInfo.Author.When, _ = time.Parse(time.RFC3339, xrpcResp.LastCommit.Author.When)
		}
	}

	user := rp.oauth.GetMultiAccountUser(r)
	rp.pages.RepoTree(w, pages.RepoTreeParams{
		BaseParams:     pages.BaseParamsFromContext(r.Context()),
		BreadCrumbs:    breadcrumbs,
		Path:           treePath,
		RepoInfo:       rp.repoResolver.GetRepoInfo(r, user),
		EmailToDid:     emailToDidMap,
		LastCommitInfo: lastCommitInfo,
		Ref:            xrpcResp.Ref,
		Parent:         derefString(xrpcResp.Parent),
		DotDot:         derefString(xrpcResp.Dotdot),
		Files:          files,
		ReadmeFileName: readmeFileName,
		Readme:         readmeFileContent,
	})
}

func derefString(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}
