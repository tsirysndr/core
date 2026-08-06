package xrpc

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotserver/git"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) Merge(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "Merge")

	actorDid, ok := r.Context().Value(ActorDid).(syntax.DID)
	if !ok {
		badRequest(xrpcerr.MissingActorDidError).send(l, w)
		return
	}

	var data tangled.RepoMerge_Input
	if err := json.NewDecoder(r.Body).Decode(&data); err != nil {
		badRequest(xrpcerr.GenericError(err)).send(l, w)
		return
	}

	repo, denial := x.pushableRepoDID(actorDid, data.Repo)
	if denial != nil {
		denial.send(l, w)
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
	if data.AuthorName != nil {
		mo.AuthorName = *data.AuthorName
	}
	if data.AuthorEmail != nil {
		mo.AuthorEmail = *data.AuthorEmail
	}
	if data.CommitBody != nil {
		mo.CommitBody = *data.CommitBody
	}
	if data.CommitMessage != nil {
		mo.CommitMessage = *data.CommitMessage
	}

	mo.CommitterName = x.Config.Git.UserName
	mo.CommitterEmail = x.Config.Git.UserEmail
	mo.FormatPatch = patchutil.IsFormatPatch(data.Patch)

	err = gr.MergeWithOptions(data.Patch, data.Branch, mo)
	if err != nil {
		var mergeErr *git.ErrMerge
		if errors.As(err, &mergeErr) {
			conflicts := make([]types.ConflictInfo, len(mergeErr.Conflicts))
			for i, conflict := range mergeErr.Conflicts {
				conflicts[i] = types.ConflictInfo{
					Filename: conflict.Filename,
					Reason:   conflict.Reason,
				}
			}

			conflictErr := xrpcerr.NewXrpcError(
				xrpcerr.WithTag("MergeConflict"),
				xrpcerr.WithMessage(fmt.Sprintf("Merge failed due to conflicts: %s", mergeErr.Message)),
			)
			conflicted(conflictErr).send(l, w)
			return
		} else {
			l.Error("failed to merge", "error", err.Error())
			serverError(xrpcerr.GitError(err)).send(l, w)
			return
		}
	}

	w.WriteHeader(http.StatusOK)
}
