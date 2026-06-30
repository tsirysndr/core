package pulls

import (
	"encoding/json"
	"fmt"
	"net/http"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/orm"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"
	"tangled.org/core/xrpc"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
)

func (s *Pulls) ResubmitPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ResubmitPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	switch r.Method {
	case http.MethodGet:
		s.pages.PullResubmitFragment(w, pages.PullResubmitParams{
			RepoInfo: s.repoResolver.GetRepoInfo(r, user),
			Pull:     pull,
		})
		return
	case http.MethodPost:
		if pull.IsPatchBased() {
			s.resubmitPatch(w, r)
			return
		} else if pull.IsBranchBased() {
			s.resubmitBranch(w, r)
			return
		} else if pull.IsForkBased() {
			s.resubmitFork(w, r)
			return
		}
	}
}

func (s *Pulls) resubmitPatch(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "resubmitPatch")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	if user == nil || user.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	patch := r.FormValue("patch")

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Did), pull, patch, "", "")
}

func (s *Pulls) resubmitBranch(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "resubmitBranch")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "resubmit-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "target_branch", pull.TargetBranch)

	if user == nil || user.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	roles := s.acl.RolesInRepo(r.Context(), f, user.Did)
	if !roles.IsPushAllowed() {
		l.Warn("unauthorized user - no push permission")
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	xrpcc := s.knotClient(f.Knot)

	xrpcBytes, err := tangled.RepoCompare(r.Context(), xrpcc, f.RepoIdentifier(), pull.TargetBranch, pull.PullSource.Branch)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.compare", "xrpcerr", xrpcerr, "err", err, "source_branch", pull.PullSource.Branch)
			s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
			return
		}
		l.Error("compare request failed", "err", err, "source_branch", pull.PullSource.Branch)
		s.pages.Notice(w, "resubmit-error", err.Error())
		return
	}

	var comparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(xrpcBytes, &comparison); err != nil {
		l.Error("failed to decode XRPC compare response", "err", err)
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	sourceRev := comparison.Rev2
	patch := comparison.FormatPatchRaw
	combined := comparison.CombinedPatchRaw

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Did), pull, patch, combined, sourceRev)
}

func (s *Pulls) resubmitFork(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "resubmitFork")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "resubmit-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "target_branch", pull.TargetBranch)

	if user == nil || user.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	forkRepo, err := db.GetRepoByDid(s.db, string(*pull.PullSource.RepoDid))
	if err != nil {
		l.Error("failed to get source repo", "err", err, "repo_did", pull.PullSource.RepoDid.String())
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	// update the hidden tracking branch to latest
	client, err := s.oauth.ServiceClient(
		r,
		oauth.WithService(forkRepo.Knot),
		oauth.WithLxm(tangled.RepoHiddenRefNSID),
		oauth.WithDev(s.config.Core.Dev),
	)
	if err != nil {
		l.Error("failed to connect to knot server", "err", err, "fork_knot", forkRepo.Knot)
		return
	}

	resp, err := tangled.RepoHiddenRef(
		r.Context(),
		client,
		&tangled.RepoHiddenRef_Input{
			ForkRef:   pull.PullSource.Branch,
			RemoteRef: pull.TargetBranch,
			Repo:      forkRepo.RepoAt().String(),
		},
	)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to set hidden ref", "xrpcerr", xrpcerr, "err", err)
		s.pages.Notice(w, "resubmit-error", xrpcerr.Error())
		return
	}
	if !resp.Success {
		l.Error("failed to update tracking ref", "err", resp.Error, "fork_ref", pull.PullSource.Branch, "remote_ref", pull.TargetBranch)
		s.pages.Notice(w, "resubmit-error", "Failed to update tracking ref.")
		return
	}

	hiddenRef := fmt.Sprintf("hidden/%s/%s", pull.PullSource.Branch, pull.TargetBranch)
	// extract patch by performing compare
	forkXrpcBytes, err := tangled.RepoCompare(r.Context(), s.knotClient(forkRepo.Knot), forkRepo.RepoIdentifier(), hiddenRef, pull.PullSource.Branch)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.compare for fork", "xrpcerr", xrpcerr, "err", err, "hidden_ref", hiddenRef, "source_branch", pull.PullSource.Branch)
			s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
			return
		}
		l.Error("failed to compare branches", "err", err, "hidden_ref", hiddenRef, "source_branch", pull.PullSource.Branch)
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	var forkComparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(forkXrpcBytes, &forkComparison); err != nil {
		l.Error("failed to decode XRPC compare response for fork", "err", err)
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	// Use the fork comparison we already made
	comparison := forkComparison

	sourceRev := comparison.Rev2
	patch := comparison.FormatPatchRaw
	combined := comparison.CombinedPatchRaw

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Did), pull, patch, combined, sourceRev)
}

func (s *Pulls) resubmitPullHelper(
	w http.ResponseWriter,
	r *http.Request,
	repo *models.Repo,
	userDid syntax.DID,
	pull *models.Pull,
	patch string,
	combined string,
	sourceRev string,
) {
	l := s.logger.With("handler", "resubmitPullHelper", "user", userDid, "pull_id", pull.PullId, "target_branch", pull.TargetBranch)

	stack := r.Context().Value("stack").(models.Stack)
	if stack != nil && len(stack) != 1 {
		l.Info("resubmitting stacked PR", "stack_size", len(stack))
		s.resubmitStackedPullHelper(w, r, repo, userDid, pull, patch)
		return
	}

	if err := validatePatch(&patch); err != nil {
		s.pages.Notice(w, "resubmit-error", err.Error())
		return
	}

	if patch == pull.LatestPatch() {
		s.pages.Notice(w, "resubmit-error", "Patch is identical to previous submission.")
		return
	}

	// validate sourceRev if branch/fork based
	if pull.IsBranchBased() || pull.IsForkBased() {
		if sourceRev == pull.LatestSha() {
			s.pages.Notice(w, "resubmit-error", "This branch has not changed since the last submission.")
			return
		}
	}

	pullAt := pull.AtUri()
	newRoundNumber := len(pull.Submissions)
	newPatch := patch
	newSourceRev := sourceRev
	combinedPatch := combined

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoPullNSID, userDid.String(), pull.Rkey)
	if err != nil {
		// failed to get record
		l.Error("failed to get record from PDS", "err", err, "rkey", pull.Rkey)
		s.pages.Notice(w, "resubmit-error", "Failed to update pull, no record found on PDS.")
		return
	}

	blob, err := xrpc.RepoUploadBlob(r.Context(), client, gz(patch), ApplicationGzip)
	if err != nil {
		l.Error("failed to upload patch blob", "err", err)
		s.pages.Notice(w, "resubmit-error", "Failed to update pull request on the PDS. Try again later.")
		return
	}
	record := pull.AsRecord()
	record.Rounds = append(record.Rounds, &tangled.RepoPull_Round{
		CreatedAt: time.Now().Format(time.RFC3339),
		PatchBlob: blob.Blob,
	})

	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoPullNSID,
		Repo:       userDid.String(),
		Rkey:       pull.Rkey,
		SwapRecord: ex.Cid,
		Record:     knotcompat.Pull(&record),
	})
	if err != nil {
		l.Error("failed to update record on PDS", "err", err, "rkey", pull.Rkey)
		s.pages.Notice(w, "resubmit-error", "Failed to update pull request on the PDS. Try again later.")
		return
	}

	err = db.ResubmitPull(s.db, pullAt, newRoundNumber, newPatch, combinedPatch, newSourceRev, blob.Blob)
	if err != nil {
		l.Error("failed to resubmit pull request in database", "err", err, "round_number", newRoundNumber)
		s.pages.Notice(w, "resubmit-error", "Failed to create pull request. Try again later.")
		return
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) resubmitStackedPullHelper(
	w http.ResponseWriter,
	r *http.Request,
	repo *models.Repo,
	userDid syntax.DID,
	pull *models.Pull,
	patch string,
) {
	l := s.logger.With("handler", "resubmitStackedPullHelper", "user", userDid, "pull_id", pull.PullId, "target_branch", pull.TargetBranch)

	targetBranch := pull.TargetBranch

	origStack, _ := r.Context().Value("stack").(models.Stack)

	formatPatches, err := patchutil.ExtractPatches(patch)
	if err != nil {
		l.Error("failed to extract patches", "err", err)
		s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Failed to parse patches.")
		return
	}

	//  must have atleast 1 patch to begin with
	if len(formatPatches) == 0 {
		l.Error("no patches found in the generated format-patch")
		s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request: No patches found in the generated patch.")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	// first upload all blobs
	blobs := make([]*lexutil.LexBlob, len(formatPatches))
	for i, p := range formatPatches {
		blob, err := xrpc.RepoUploadBlob(r.Context(), client, gz(p.Raw), ApplicationGzip)
		if err != nil {
			l.Error("failed to upload patch blob", "err", err, "patch_index", i)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}
		l.Info("uploaded blob", "idx", i+1, "total", len(formatPatches))
		blobs[i] = blob.Blob
	}

	newStack, err := s.newStack(r.Context(), repo, userDid, targetBranch, pull.PullSource, formatPatches, blobs, nil, nil)
	if err != nil {
		l.Error("failed to create resubmitted stack", "err", err)
		s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
		return
	}

	// find the diff between the stacks, first, map them by changeId
	origById := make(map[string]*models.Pull)
	newById := make(map[string]*models.Pull)
	for _, p := range origStack {
		origById[p.LatestSubmission().ChangeId()] = p
	}
	for _, p := range newStack {
		newById[p.LatestSubmission().ChangeId()] = p
	}

	// commits that got deleted: corresponding pull is closed
	// commits that got added: new pull is created
	// commits that got updated: corresponding pull is resubmitted & new round begins
	additions := make(map[string]*models.Pull)
	deletions := make(map[string]*models.Pull)
	updated := make(map[string]struct{})

	// pulls in original stack but not in new one
	for _, op := range origStack {
		if _, ok := newById[op.LatestSubmission().ChangeId()]; !ok {
			deletions[op.LatestSubmission().ChangeId()] = op
		}
	}

	// pulls in new stack but not in original one
	for _, np := range newStack {
		if _, ok := origById[np.LatestSubmission().ChangeId()]; !ok {
			additions[np.LatestSubmission().ChangeId()] = np
		}
	}

	// NOTE: this loop can be written in any of above blocks,
	// but is written separately in the interest of simpler code
	for _, np := range newStack {
		if op, ok := origById[np.LatestSubmission().ChangeId()]; ok {
			// pull exists in both stacks
			updated[op.LatestSubmission().ChangeId()] = struct{}{}
		}
	}

	// NOTE: we can go through the newStack and update dependent relations and
	// rkeys now that we know which ones have been updated
	// update dependentOn relations for the entire stack
	var parentAt *syntax.ATURI
	for _, np := range newStack {
		if op, ok := origById[np.LatestSubmission().ChangeId()]; ok {
			// pull exists in both stacks
			np.Rkey = op.Rkey
		}
		np.DependentOn = parentAt
		x := np.AtUri()
		parentAt = &x
	}

	l = l.With("additions", len(additions), "deletions", len(deletions), "updates", len(updated))

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
		return
	}
	defer tx.Rollback()

	// pds updates to make
	var writes []*comatproto.RepoApplyWrites_Input_Writes_Elem

	// deleted pulls are marked as deleted in the DB
	for _, p := range deletions {
		// do not do delete already merged PRs
		if p.State == models.PullMerged {
			continue
		}

		err := db.AbandonPulls(tx, orm.FilterEq("repo_did", string(p.RepoDid)), orm.FilterEq("at_uri", p.AtUri()))
		if err != nil {
			l.Error("failed to delete pull", "err", err, "pull_id", p.PullId)
			s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
			return
		}
		writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
			RepoApplyWrites_Delete: &comatproto.RepoApplyWrites_Delete{
				Collection: tangled.RepoPullNSID,
				Rkey:       p.Rkey,
			},
		})
	}

	// new pulls are created
	for _, p := range additions {
		blob, err := xrpc.RepoUploadBlob(r.Context(), client, gz(p.LatestPatch()), ApplicationGzip)
		if err != nil {
			l.Error("failed to upload patch blob for new pull", "err", err, "change_id", p.LatestSubmission().ChangeId())
			s.pages.Notice(w, "resubmit-error", "Failed to update pull request on the PDS. Try again later.")
			return
		}
		p.Submissions[0].Blob = *blob.Blob

		if err = db.PutPull(tx, p); err != nil {
			l.Error("failed to create pull", "err", err, "pull_id", p.PullId, "change_id", p.LatestSubmission().ChangeId())
			s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
			return
		}

		record := p.AsRecord()
		record.Rounds = []*tangled.RepoPull_Round{
			{
				CreatedAt: time.Now().Format(time.RFC3339),
				PatchBlob: blob.Blob,
			},
		}
		writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
			RepoApplyWrites_Create: &comatproto.RepoApplyWrites_Create{
				Collection: tangled.RepoPullNSID,
				Rkey:       &p.Rkey,
				Value:      knotcompat.Pull(&record),
			},
		})
	}

	// updated pulls are, well, updated; to start a new round
	for id := range updated {
		op, _ := origById[id]
		np, _ := newById[id]

		// do not update already merged PRs
		if op.State == models.PullMerged {
			continue
		}

		// resubmit the new pull
		np.Rkey = op.Rkey
		pullAt := op.AtUri()
		newRoundNumber := len(op.Submissions)
		newPatch := np.LatestPatch()
		combinedPatch := np.LatestSubmission().Combined
		newSourceRev := np.LatestSha()

		blob, err := xrpc.RepoUploadBlob(r.Context(), client, gz(newPatch), ApplicationGzip)
		if err != nil {
			l.Error("failed to upload patch blob for update", "err", err, "change_id", id, "pull_id", op.PullId)
			s.pages.Notice(w, "resubmit-error", "Failed to update pull request on the PDS. Try again later.")
			return
		}

		// create new round
		err = db.ResubmitPull(tx, pullAt, newRoundNumber, newPatch, combinedPatch, newSourceRev, blob.Blob)
		if err != nil {
			l.Error("failed to update pull in database", "err", err, "pull_id", op.PullId, "round_number", newRoundNumber)
			s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
			return
		}

		// update dependent-on relation
		if np.DependentOn != nil {
			err := db.SetDependentOn(tx, *np.DependentOn, orm.FilterEq("at_uri", np.AtUri()))
			if err != nil {
				l.Error("failed to update pull in database", "err", err, "pull_id", op.PullId, "round_number", newRoundNumber)
				s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
				return
			}
		}

		record := np.AsRecord()
		record.Rounds = op.AsRecord().Rounds
		record.Rounds = append(record.Rounds, &tangled.RepoPull_Round{
			CreatedAt: time.Now().Format(time.RFC3339),
			PatchBlob: blob.Blob,
		})
		writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
			RepoApplyWrites_Update: &comatproto.RepoApplyWrites_Update{
				Collection: tangled.RepoPullNSID,
				Rkey:       op.Rkey,
				Value:      knotcompat.Pull(&record),
			},
		})
	}

	_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
		Repo:   userDid.String(),
		Writes: writes,
	})
	if err != nil {
		l.Error("failed to apply writes for stacked pull request", "err", err, "writes_count", len(writes))
		s.pages.Notice(w, "pull", "Failed to create stacked pull request. Try again later.")
		return
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to commit resubmit transaction", "err", err)
		s.pages.Notice(w, "pull-resubmit-error", "Failed to resubmit pull request. Try again later.")
		return
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}
