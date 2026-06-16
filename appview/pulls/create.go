package pulls

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/patchutil"
	"tangled.org/core/tid"
	"tangled.org/core/types"
	"tangled.org/core/xrpc"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
)

func (s *Pulls) handleBranchBasedPull(
	w http.ResponseWriter,
	r *http.Request,
	repo *models.Repo,
	userDid syntax.DID,
	title,
	body,
	targetBranch,
	sourceBranch string,
	isStacked bool,
	stackTitles, stackBodies map[string]string,
) {
	l := s.logger.With("handler", "handleBranchBasedPull", "user", userDid, "target_branch", targetBranch, "source_branch", sourceBranch, "is_stacked", isStacked)

	xrpcc := s.knotClient(repo.Knot)

	xrpcBytes, err := tangled.RepoCompare(r.Context(), xrpcc, repo.RepoIdentifier(), targetBranch, sourceBranch)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.compare", "xrpcerr", xrpcerr, "err", err)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}
		l.Error("failed to compare", "err", err)
		s.pages.Notice(w, "pull", err.Error())
		return
	}

	var comparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(xrpcBytes, &comparison); err != nil {
		l.Error("failed to decode XRPC compare response", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	if len(comparison.FormatPatch) == 0 {
		s.pages.Notice(w, "pull", "No commits between target and source.")
		return
	}

	sourceRev := comparison.Rev2
	patch := comparison.FormatPatchRaw
	combined := comparison.CombinedPatchRaw

	if err := s.validator.ValidatePatch(&patch); err != nil {
		s.logger.Error("failed to validate patch", "err", err)
		s.pages.Notice(w, "pull", "Invalid patch format. Please provide a valid diff.")
		return
	}

	pullSource := &models.PullSource{
		Branch: sourceBranch,
	}

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, combined, sourceRev, pullSource, isStacked, stackTitles, stackBodies)
}

func (s *Pulls) handlePatchBasedPull(w http.ResponseWriter, r *http.Request, repo *models.Repo, userDid syntax.DID, title, body, targetBranch, patch string, isStacked bool, stackTitles, stackBodies map[string]string) {
	if err := s.validator.ValidatePatch(&patch); err != nil {
		s.logger.Error("patch validation failed", "err", err)
		s.pages.Notice(w, "pull", "Invalid patch format. Please provide a valid diff.")
		return
	}

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, "", "", nil, isStacked, stackTitles, stackBodies)
}

func (s *Pulls) handleForkBasedPull(w http.ResponseWriter, r *http.Request, repo *models.Repo, userDid syntax.DID, forkRepoDid string, title, body, targetBranch, sourceBranch string, isStacked bool, stackTitles, stackBodies map[string]string) {
	l := s.logger.With("handler", "handleForkBasedPull", "user", userDid, "fork_repo_did", forkRepoDid, "target_branch", targetBranch, "source_branch", sourceBranch, "is_stacked", isStacked)

	if forkRepoDid == "" {
		s.pages.Notice(w, "pull", "No such fork.")
		return
	}
	fork, err := db.GetForkByRepoDid(s.db, forkRepoDid)
	if errors.Is(err, sql.ErrNoRows) {
		s.pages.Notice(w, "pull", "No such fork.")
		return
	} else if err != nil {
		l.Error("failed to fetch fork", "err", err, "fork_repo_did", forkRepoDid)
		s.pages.Notice(w, "pull", "Failed to fetch fork.")
		return
	}

	client, err := s.oauth.ServiceClient(
		r,
		oauth.WithService(fork.Knot),
		oauth.WithLxm(tangled.RepoHiddenRefNSID),
		oauth.WithDev(s.config.Core.Dev),
	)

	resp, err := tangled.RepoHiddenRef(
		r.Context(),
		client,
		&tangled.RepoHiddenRef_Input{
			ForkRef:   sourceBranch,
			RemoteRef: targetBranch,
			Repo:      fork.RepoAt().String(),
		},
	)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to set hidden ref", "xrpcerr", xrpcerr, "err", err)
		s.pages.Notice(w, "pull", xrpcerr.Error())
		return
	}

	if !resp.Success {
		errorMsg := "Failed to create pull request"
		if resp.Error != nil {
			errorMsg = fmt.Sprintf("Failed to create pull request: %s", *resp.Error)
		}
		s.pages.Notice(w, "pull", errorMsg)
		return
	}

	hiddenRef := fmt.Sprintf("hidden/%s/%s", sourceBranch, targetBranch)
	// We're now comparing the sourceBranch (on the fork) against the hiddenRef which is tracking
	// the targetBranch on the target repository. This code is a bit confusing, but here's an example:
	// hiddenRef: hidden/feature-1/main (on repo-fork)
	// targetBranch: main (on repo-1)
	// sourceBranch: feature-1 (on repo-fork)
	forkXrpcc := s.knotClient(fork.Knot)

	forkXrpcBytes, err := tangled.RepoCompare(r.Context(), forkXrpcc, fork.RepoIdentifier(), hiddenRef, sourceBranch)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.compare for fork", "xrpcerr", xrpcerr, "err", err, "hidden_ref", hiddenRef)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}
		l.Error("failed to compare across branches", "err", err, "hidden_ref", hiddenRef)
		s.pages.Notice(w, "pull", err.Error())
		return
	}

	var comparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(forkXrpcBytes, &comparison); err != nil {
		l.Error("failed to decode XRPC compare response for fork", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	if len(comparison.FormatPatch) == 0 {
		s.pages.Notice(w, "pull", "No commits between target and source.")
		return
	}

	sourceRev := comparison.Rev2
	patch := comparison.FormatPatchRaw
	combined := comparison.CombinedPatchRaw

	if err := s.validator.ValidatePatch(&patch); err != nil {
		s.logger.Error("failed to validate patch", "err", err)
		s.pages.Notice(w, "pull", "Invalid patch format. Please provide a valid diff.")
		return
	}

	forkDid := syntax.DID(fork.RepoDid)
	pullSource := &models.PullSource{
		Branch:  sourceBranch,
		RepoDid: &forkDid,
	}

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, combined, sourceRev, pullSource, isStacked, stackTitles, stackBodies)
}

func (s *Pulls) createPullRequest(
	w http.ResponseWriter,
	r *http.Request,
	repo *models.Repo,
	userDid syntax.DID,
	title, body, targetBranch string,
	patch string,
	combined string,
	sourceRev string,
	pullSource *models.PullSource,
	isStacked bool,
	stackTitles, stackBodies map[string]string,
) {
	l := s.logger.With("handler", "createPullRequest", "user", userDid, "target_branch", targetBranch, "is_stacked", isStacked)

	if isStacked {
		// creates a series of PRs, each linking to the previous, identified by jj's change-id
		s.createStackedPullRequest(
			w,
			r,
			repo,
			userDid,
			targetBranch,
			patch,
			sourceRev,
			pullSource,
			stackTitles,
			stackBodies,
		)
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start tx", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}
	defer tx.Rollback()

	// We've already checked earlier if it's diff-based and title is empty,
	// so if it's still empty now, it's intentionally skipped owing to format-patch.
	if title == "" || body == "" {
		formatPatches, err := patchutil.ExtractPatches(patch)
		if err != nil {
			s.pages.Notice(w, "pull", fmt.Sprintf("Failed to extract patches: %v", err))
			return
		}
		if len(formatPatches) == 0 {
			s.pages.Notice(w, "pull", "No patches found in the supplied format-patch.")
			return
		}

		if title == "" {
			title = formatPatches[0].Title
		}
		if body == "" {
			body = formatPatches[0].Body
		}
	}

	mentions, references := s.mentionsResolver.Resolve(r.Context(), body)

	rkey := tid.TID()

	blob, err := xrpc.RepoUploadBlob(r.Context(), client, gz(patch), ApplicationGzip)
	if err != nil {
		l.Error("failed to upload patch", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	now := time.Now()

	pull := &models.Pull{
		Title:        title,
		Body:         body,
		TargetBranch: targetBranch,
		OwnerDid:     userDid.String(),
		RepoDid:      syntax.DID(repo.RepoDid),
		Rkey:         rkey,
		Mentions:     mentions,
		References:   references,
		Submissions: []*models.PullSubmission{
			{
				Patch:     patch,
				Combined:  combined,
				SourceRev: sourceRev,
				Blob:      *blob.Blob,
				Created:   now,
			},
		},
		PullSource: pullSource,
		State:      models.PullOpen,
		Created:    now,
		Repo:       repo,
	}

	record := pull.AsRecord()
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoPullNSID,
		Repo:       userDid.String(),
		Rkey:       rkey,
		Record:     knotcompat.Pull(&record),
	})
	if err != nil {
		l.Error("failed to create pull request", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	err = db.PutPull(tx, pull)
	if err != nil {
		l.Error("failed to create pull request in database", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}
	pullId, err := db.NextPullId(tx, repo.RepoDid)
	if err != nil {
		s.logger.Error("failed to get pull id", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction for pull request", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	s.notifier.NewPull(r.Context(), pull)

	s.applyCreationLabels(r.Context(), client, userDid, []*models.Pull{pull}, r.Form, repo)

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxRedirect(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pullId))
}

func (s *Pulls) createStackedPullRequest(
	w http.ResponseWriter,
	r *http.Request,
	repo *models.Repo,
	userDid syntax.DID,
	targetBranch string,
	patch string,
	sourceRev string,
	pullSource *models.PullSource,
	stackTitles, stackBodies map[string]string,
) {
	l := s.logger.With("handler", "createStackedPullRequest", "user", userDid, "target_branch", targetBranch, "source_rev", sourceRev)

	// run some necessary checks for stacked-prs first

	formatPatches, err := patchutil.ExtractPatches(patch)
	if err != nil {
		l.Error("failed to extract patches", "err", err)
		s.pages.Notice(w, "pull", fmt.Sprintf("Failed to extract patches: %v", err))
		return
	}

	//  must have atleast 1 patch to begin with
	if len(formatPatches) == 0 {
		l.Error("empty patches")
		s.pages.Notice(w, "pull", "No patches found in the generated format-patch.")
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

	// build a stack out of this patch
	stack, err := s.newStack(r.Context(), repo, userDid, targetBranch, pullSource, formatPatches, blobs, stackTitles, stackBodies)
	if err != nil {
		l.Error("failed to create stack", "err", err)
		s.pages.Notice(w, "pull", fmt.Sprintf("Failed to create stack: %v", err))
		return
	}

	// apply all record creations at once
	var writes []*comatproto.RepoApplyWrites_Input_Writes_Elem
	for _, p := range stack {
		record := p.AsRecord()
		writes = append(writes, &comatproto.RepoApplyWrites_Input_Writes_Elem{
			RepoApplyWrites_Create: &comatproto.RepoApplyWrites_Create{
				Collection: tangled.RepoPullNSID,
				Rkey:       &p.Rkey,
				Value:      knotcompat.Pull(&record),
			},
		})
	}
	_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
		Repo:   userDid.String(),
		Writes: writes,
	})
	if err != nil {
		l.Error("failed to create stacked pull request", "err", err)
		s.pages.Notice(w, "pull", "Failed to create stacked pull request. Try again later.")
		return
	}

	// create all pulls at once
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start tx", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}
	defer tx.Rollback()

	for _, p := range stack {
		err = db.PutPull(tx, p)
		if err != nil {
			l.Error("failed to create pull request in database", "err", err, "pull_rkey", p.Rkey)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}

	}

	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction for pull requests", "err", err)
		s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
		return
	}

	// notify about each pull
	//
	// this is performed after tx.Commit, because it could result in a locked DB otherwise
	for _, p := range stack {
		s.notifier.NewPull(r.Context(), p)
	}

	s.applyCreationLabels(r.Context(), client, userDid, stack, r.Form, repo)

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxRedirect(w, fmt.Sprintf("/%s/pulls", ownerSlashRepo))
}

func (s *Pulls) newStack(
	ctx context.Context,
	repo *models.Repo,
	userDid syntax.DID,
	targetBranch string,
	pullSource *models.PullSource,
	formatPatches []types.FormatPatch,
	blobs []*lexutil.LexBlob,
	stackTitles, stackBodies map[string]string,
) (models.Stack, error) {
	var stack models.Stack
	var parentAtUri *syntax.ATURI
	for i, fp := range formatPatches {
		//  all patches must have a jj change-id
		cid, err := fp.ChangeId()
		if err != nil {
			return nil, fmt.Errorf("Stacking is only supported if all patches contain a change-id commit header.")
		}

		title := fp.Title
		body := fp.Body
		if override, ok := stackTitles[cid]; ok && strings.TrimSpace(override) != "" {
			title = override
		}
		if override, ok := stackBodies[cid]; ok {
			body = override
		}
		rkey := tid.TID()

		mentions, references := s.mentionsResolver.Resolve(ctx, body)

		now := time.Now()

		pull := models.Pull{
			Title:        title,
			Body:         body,
			TargetBranch: targetBranch,
			OwnerDid:     userDid.String(),
			RepoDid:      syntax.DID(repo.RepoDid),
			Rkey:         rkey,
			Mentions:     mentions,
			References:   references,
			Submissions: []*models.PullSubmission{
				{
					Patch:     fp.Raw,
					SourceRev: fp.SHA,
					Combined:  fp.Raw,
					Blob:      *blobs[i],
					Created:   now,
				},
			},
			PullSource: pullSource,
			Created:    now,
			State:      models.PullOpen,

			DependentOn: parentAtUri,
			Repo:        repo,
		}

		stack = append(stack, &pull)

		parent := pull.AtUri()
		parentAtUri = &parent
	}

	return stack, nil
}
