package xrpc

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"

	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/patchutil"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) MergeCheck(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "MergeCheck")

	var data tangled.RepoMergeCheck_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	repo, err := x.resolveRepoDID(data.Repo)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		badRequest(xrpcerr.RepoNotFoundError).send(l, w)
		return
	}

	gr, err := git.Open(repo.path, data.Branch)
	if err != nil {
		badRequest(xrpcerr.GenericError(fmt.Errorf("failed to open repository: %w", err))).send(l, w)
		return
	}
	if x.Sandbox != nil {
		gr = gr.WithSandbox(x.Sandbox)
	}

	mo := git.MergeOptions{}
	mo.CommitMessage = "merge check"
	mo.CommitterName = x.Config.Git.UserName
	mo.CommitterEmail = x.Config.Git.UserEmail
	mo.FormatPatch = patchutil.IsFormatPatch(data.Patch)

	err = gr.MergeCheckWithOptions(data.Patch, data.Branch, mo)

	response := tangled.RepoMergeCheck_Output{
		Is_conflicted: false,
	}

	if err != nil {
		var mergeErr *git.ErrMerge
		if errors.As(err, &mergeErr) {
			response.Is_conflicted = true

			conflicts := make([]*tangled.RepoMergeCheck_ConflictInfo, len(mergeErr.Conflicts))
			for i, conflict := range mergeErr.Conflicts {
				conflicts[i] = &tangled.RepoMergeCheck_ConflictInfo{
					Filename: conflict.Filename,
					Reason:   conflict.Reason,
				}
			}
			response.Conflicts = conflicts

			if mergeErr.Message != "" {
				response.Message = &mergeErr.Message
			}
		} else {
			response.Is_conflicted = true
			errMsg := err.Error()
			response.Error = &errMsg
		}
	}

	l.Debug("merge check response", "isConflicted", response.Is_conflicted, "err", response.Error, "conflicts", response.Conflicts)

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(response)
}
