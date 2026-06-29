package pulls

import (
	"fmt"
	"net/http"
	"strconv"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/orm"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
)

// htmx fragment
func (s *Pulls) PullActions(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullActions")

	switch r.Method {
	case http.MethodGet:
		user := s.oauth.GetMultiAccountUser(r)
		if user != nil {
			l = l.With("user", user.Did)
		}

		f, err := s.repoResolver.Resolve(r)
		if err != nil {
			l.Error("failed to get repo and knot", "err", err)
			return
		}

		pull, ok := r.Context().Value("pull").(*models.Pull)
		if !ok {
			l.Error("failed to get pull")
			s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
			return
		}
		l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

		// can be nil  if this pull is not stacked
		stack, _ := r.Context().Value("stack").(models.Stack)

		roundNumberStr := chi.URLParam(r, "round")
		roundNumber, err := strconv.Atoi(roundNumberStr)
		if err != nil {
			roundNumber = pull.LastRoundNumber()
		}
		if roundNumber >= len(pull.Submissions) {
			http.Error(w, "bad round id", http.StatusBadRequest)
			l.Error("failed to parse round id", "err", err, "round_number", roundNumber)
			return
		}

		// only the last round's buttons and banners use merge/resubmit checks
		isLastRound := roundNumber == pull.LastRoundNumber()
		branchDeleteStatus := s.branchDeleteStatus(r, f, pull)
		mergeCheckResponse := types.MergeCheckResponse{}
		resubmitResult := pages.Unknown
		if isLastRound {
			mergeCheckResponse = s.mergeCheck(r, f, pull, stack)
			if user != nil && user.Did == pull.OwnerDid {
				resubmitResult = s.resubmitCheck(r, f, pull, stack)
			}
		}

		s.pages.PullActionsFragment(w, pages.PullActionsParams{
			BaseParams:         pages.BaseParamsFromContext(r.Context()),
			RepoInfo:           s.repoResolver.GetRepoInfo(r, user),
			Pull:               pull,
			RoundNumber:        roundNumber,
			MergeCheck:         mergeCheckResponse,
			ResubmitCheck:      resubmitResult,
			BranchDeleteStatus: branchDeleteStatus,
			Stack:              stack,
		})
		return
	}
}

func (s *Pulls) repoPullHelper(w http.ResponseWriter, r *http.Request, interdiff bool) {
	l := s.logger.With("handler", "repoPullHelper", "interdiff", interdiff)

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	if user != nil {
		userDid := user.Did
		repoDid := f.RepoDid
		pullId := pull.PullId
		atUri := pull.AtUri().String()
		focusing := pages.BaseParamsFromContext(r.Context()).FocusParams.Focusing
		go func() {
			if !focusing {
				if err := db.MarkNotificationsReadForPull(s.db, userDid, repoDid, pullId); err != nil {
					l.Error("failed to mark pull notifications as read", "err", err)
				}
			}
			if err := db.UpsertRecentLink(s.db, userDid, models.RecentLinkTypePull, atUri); err != nil {
				l.Error("failed to upsert recent link", "err", err)
			}
		}()
	}

	backlinks, err := db.GetBacklinks(s.db, pull.AtUri())
	if err != nil {
		l.Error("failed to get pull backlinks", "err", err)
		s.pages.Notice(w, "pull-error", "Failed to get pull. Try again later.")
		return
	}

	roundId := chi.URLParam(r, "round")
	roundIdInt := pull.LastRoundNumber()
	if r, err := strconv.Atoi(roundId); err == nil {
		roundIdInt = r
	}
	if roundIdInt >= len(pull.Submissions) {
		http.Error(w, "bad round id", http.StatusBadRequest)
		l.Error("failed to parse round id", "err", err, "round_number", roundIdInt)
		return
	}

	var diffOpts types.DiffOpts
	if d := r.URL.Query().Get("diff"); d == "split" {
		diffOpts.Split = true
	}

	// can be nil  if this pull is not stacked
	stack, _ := r.Context().Value("stack").(models.Stack)

	m := make(map[string]models.Pipeline)

	var shas []string
	for _, s := range pull.Submissions {
		shas = append(shas, s.SourceRev)
	}
	for _, p := range stack {
		shas = append(shas, p.LatestSha())
	}

	ps, err := db.GetPipelineStatuses(
		s.db,
		len(shas),
		orm.FilterEq("p.repo_did", f.RepoDid),
		orm.FilterIn("p.sha", shas),
	)
	if err != nil {
		l.Error("failed to fetch pipeline statuses", "err", err)
		// non-fatal
	}

	for _, p := range ps {
		m[p.Sha] = p
	}

	entities := []syntax.ATURI{pull.AtUri()}
	for _, s := range pull.Submissions {
		for _, c := range s.Comments {
			entities = append(entities, c.FeedCommentAtUri())
		}
	}
	reactions, err := db.ListReactionDisplayDataMap(s.db, entities, 20)
	if err != nil {
		l.Error("failed to get pull reactions", "err", err)
	}

	var userReactions map[syntax.ATURI]map[models.ReactionKind]bool
	if user != nil {
		userReactions, err = db.ListReactionStatusMap(s.db, entities, syntax.DID(user.Did))
		if err != nil {
			s.logger.Error("failed to get user reactions", "err", err)
		}
	}

	labelDefs, err := db.GetLabelDefinitions(
		s.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", tangled.RepoPullNSID),
	)
	if err != nil {
		l.Error("failed to fetch labels", "err", err)
		s.pages.Error503(w)
		return
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	vouchRelationships := make(map[syntax.DID]*models.VouchRelationship)
	vouchSkips := make(map[syntax.DID]bool)
	if user != nil {
		participants := pull.Participants()
		vouchRelationships, err = db.GetVouchRelationshipsBatch(s.db, syntax.DID(user.Did), participants)
		if err != nil {
			l.Error("failed to fetch vouch relationships", "err", err)
		}
		ownerDid := syntax.DID(pull.OwnerDid)
		skipped, err := db.IsVouchSkipped(s.db, user.Did, pull.OwnerDid)
		if err != nil {
			l.Error("failed to check vouch skip", "err", err)
		}
		vouchSkips[ownerDid] = skipped
	}

	var diff types.DiffRenderer
	if interdiff {
		currentPatch, err := patchutil.AsDiff(pull.Submissions[roundIdInt].CombinedPatch())
		if err != nil {
			l.Error("failed to interdiff; current patch malformed", "err", err, "round_number", roundIdInt)
			s.pages.Notice(w, fmt.Sprintf("interdiff-error-%d", roundIdInt), "Failed to calculate interdiff; current patch is invalid.")
			return
		}

		previousPatch, err := patchutil.AsDiff(pull.Submissions[roundIdInt-1].CombinedPatch())
		if err != nil {
			l.Error("failed to interdiff; previous patch malformed", "err", err, "round_number", roundIdInt)
			s.pages.Notice(w, fmt.Sprintf("interdiff-error-%d", roundIdInt), "Failed to calculate interdiff; previous patch is invalid.")
			return
		}

		diff = patchutil.Interdiff(previousPatch, currentPatch)
	} else {
		diff = s.combinedDiff(pull, roundIdInt)
	}

	err = s.pages.RepoSinglePull(w, pages.RepoSinglePullParams{
		BaseParams:         pages.BaseParamsFromContext(r.Context()),
		RepoInfo:           s.repoResolver.GetRepoInfo(r, user),
		Pull:               pull,
		Stack:              stack,
		Backlinks:          backlinks,
		BranchDeleteStatus: nil,
		MergeCheck:         types.MergeCheckResponse{},
		ResubmitCheck:      pages.Unknown,
		Pipelines:          m,
		Diff:               diff,
		DiffOpts:           diffOpts,
		ActiveRound:        roundIdInt,
		IsInterdiff:        interdiff,

		Reactions:   reactions,
		UserReacted: userReactions,

		LabelDefs:          defs,
		VouchRelationships: vouchRelationships,
		VouchSkips:         vouchSkips,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
}

func (s *Pulls) combinedDiff(pull *models.Pull, round int) types.DiffRenderer {
	submission := pull.Submissions[round]
	key := fmt.Sprintf("%s|%d|%s", pull.AtUri(), round, submission.SourceRev)
	if cached, ok := s.diffCache.Get(key); ok {
		return cached
	}

	diff := patchutil.AsNiceDiff(submission.CombinedPatch(), pull.TargetBranch)
	s.diffCache.Add(key, diff)
	return diff
}

func (s *Pulls) RepoSinglePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "RepoSinglePull")

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}

	http.Redirect(w, r, r.URL.String()+fmt.Sprintf("/round/%d", pull.LastRoundNumber()), http.StatusFound)
}

func (s *Pulls) mergeCheck(r *http.Request, f *models.Repo, pull *models.Pull, stack models.Stack) types.MergeCheckResponse {
	if pull.State == models.PullMerged {
		return types.MergeCheckResponse{}
	}

	xrpcc := s.knotClient(f.Knot)

	// combine patches of substack
	subStack := stack.Below(pull)
	// collect the portion of the stack that is mergeable
	mergeable := subStack.Mergeable()
	// combine each patch
	patch := mergeable.CombinedPatch()

	resp, err := tangled.RepoMergeCheck(
		r.Context(),
		xrpcc,
		&tangled.RepoMergeCheck_Input{
			Did:    f.Did,
			Name:   f.Name,
			Branch: pull.TargetBranch,
			Patch:  patch,
		},
	)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to check for mergeability", "xrpcerr", xrpcerr, "err", err, "pull_id", pull.PullId, "target_branch", pull.TargetBranch)
		return types.MergeCheckResponse{
			Error: fmt.Sprintf("failed to check merge status: %s", xrpcerr.Error()),
		}
	}

	return mergeCheckResponseFrom(resp)
}

func mergeCheckResponseFrom(resp *tangled.RepoMergeCheck_Output) types.MergeCheckResponse {
	conflicts := make([]types.ConflictInfo, len(resp.Conflicts))
	for i, c := range resp.Conflicts {
		conflicts[i] = types.ConflictInfo{Filename: c.Filename, Reason: c.Reason}
	}
	out := types.MergeCheckResponse{
		IsConflicted: resp.Is_conflicted,
		Conflicts:    conflicts,
	}
	if resp.Message != nil {
		out.Message = *resp.Message
	}
	if resp.Error != nil {
		out.Error = *resp.Error
	}
	return out
}

func (s *Pulls) branchDeleteStatus(r *http.Request, repo *models.Repo, pull *models.Pull) *models.BranchDeleteStatus {
	if pull.State != models.PullMerged {
		return nil
	}

	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		return nil
	}

	var branch string
	// check if the branch exists
	// NOTE: appview could cache branches/tags etc. for every repo by listening for gitRefUpdates
	if pull.IsBranchBased() {
		branch = pull.PullSource.Branch
	} else if pull.IsForkBased() {
		branch = pull.PullSource.Branch
		repo = pull.PullSource.Repo
	} else {
		return nil
	}

	// deleted fork
	if repo == nil {
		return nil
	}

	// user can only delete branch if they are a collaborator in the repo that the branch belongs to
	if !s.acl.HasRepoPermission(r.Context(), repo, user.Did, "repo:push") {
		return nil
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	resp, err := tangled.GitTempGetBranch(r.Context(), xrpcc, branch, repo.RepoDid)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to get branch", "xrpcerr", xrpcerr, "err", err)
		return nil
	}

	return &models.BranchDeleteStatus{
		Repo:   repo,
		Branch: resp.Name,
	}
}

func (s *Pulls) resubmitCheck(r *http.Request, repo *models.Repo, pull *models.Pull, stack models.Stack) pages.ResubmitResult {
	if pull.State == models.PullMerged || pull.State == models.PullAbandoned || pull.PullSource == nil {
		return pages.Unknown
	}

	var sourceRepoDid string
	if pull.PullSource.RepoDid != nil {
		sourceRepoDid = string(*pull.PullSource.RepoDid)
	} else {
		sourceRepoDid = repo.RepoDid
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	branchResp, err := tangled.GitTempGetBranch(r.Context(), xrpcc, pull.PullSource.Branch, sourceRepoDid)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			s.logger.Error("failed to call XRPC repo.branches", "xrpcerr", xrpcerr, "err", err, "pull_id", pull.PullId, "branch", pull.PullSource.Branch)
			return pages.Unknown
		}
		s.logger.Error("failed to reach knotserver", "err", err, "pull_id", pull.PullId)
		return pages.Unknown
	}

	targetBranch := branchResp

	top := stack[0]
	latestSourceRev := top.LatestSha()

	if latestSourceRev != targetBranch.Hash {
		return pages.ShouldResubmit
	}

	return pages.ShouldNotResubmit
}

func (s *Pulls) RepoPullPatch(w http.ResponseWriter, r *http.Request) {
	s.repoPullHelper(w, r, false)
}

func (s *Pulls) RepoPullInterdiff(w http.ResponseWriter, r *http.Request) {
	s.repoPullHelper(w, r, true)
}

func (s *Pulls) RepoPullPatchRaw(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "RepoPullPatchRaw")

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId)

	roundId := chi.URLParam(r, "round")
	roundIdInt, err := strconv.Atoi(roundId)
	if err != nil || roundIdInt >= len(pull.Submissions) {
		http.Error(w, "bad round id", http.StatusBadRequest)
		l.Error("failed to parse round id", "err", err, "round_id_str", roundId)
		return
	}

	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.Write([]byte(pull.Submissions[roundIdInt].Patch))
}
