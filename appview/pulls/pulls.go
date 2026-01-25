package pulls

import (
	"bytes"
	"compress/gzip"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	pulls_indexer "tangled.org/core/appview/indexer/pulls"
	"tangled.org/core/appview/mentions"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/searchquery"
	"tangled.org/core/appview/validator"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/idresolver"
	"tangled.org/core/ogre"
	"tangled.org/core/orm"
	"tangled.org/core/patchutil"
	"tangled.org/core/rbac"
	"tangled.org/core/tid"
	"tangled.org/core/types"
	"tangled.org/core/xrpc"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
)

const ApplicationGzip = "application/gzip"

type Pulls struct {
	oauth            *oauth.OAuth
	repoResolver     *reporesolver.RepoResolver
	pages            *pages.Pages
	idResolver       *idresolver.Resolver
	mentionsResolver *mentions.Resolver
	db               *db.DB
	config           *config.Config
	notifier         notify.Notifier
	enforcer         *rbac.Enforcer
	logger           *slog.Logger
	validator        *validator.Validator
	indexer          *pulls_indexer.Indexer
	ogreClient       *ogre.Client
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	pages *pages.Pages,
	resolver *idresolver.Resolver,
	mentionsResolver *mentions.Resolver,
	db *db.DB,
	config *config.Config,
	notifier notify.Notifier,
	enforcer *rbac.Enforcer,
	validator *validator.Validator,
	indexer *pulls_indexer.Indexer,
	logger *slog.Logger,
) *Pulls {
	return &Pulls{
		oauth:            oauth,
		repoResolver:     repoResolver,
		pages:            pages,
		idResolver:       resolver,
		mentionsResolver: mentionsResolver,
		db:               db,
		config:           config,
		notifier:         notifier,
		enforcer:         enforcer,
		logger:           logger,
		validator:        validator,
		indexer:          indexer,
		ogreClient:       ogre.NewClient(config.Ogre.Host),
	}
}

// htmx fragment
func (s *Pulls) PullActions(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullActions")

	switch r.Method {
	case http.MethodGet:
		user := s.oauth.GetMultiAccountUser(r)
		if user != nil {
			l = l.With("user", user.Active.Did)
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

		mergeCheckResponse := s.mergeCheck(r, f, pull, stack)
		branchDeleteStatus := s.branchDeleteStatus(r, f, pull)
		resubmitResult := pages.Unknown
		if user.Active.Did == pull.OwnerDid {
			resubmitResult = s.resubmitCheck(r, f, pull, stack)
		}

		s.pages.PullActionsFragment(w, pages.PullActionsParams{
			LoggedInUser:       user,
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
		l = l.With("user", user.Active.Did)
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

	mergeCheckResponse := s.mergeCheck(r, f, pull, stack)
	branchDeleteStatus := s.branchDeleteStatus(r, f, pull)
	resubmitResult := pages.Unknown
	if user != nil && user.Active.Did == pull.OwnerDid {
		resubmitResult = s.resubmitCheck(r, f, pull, stack)
	}

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
		orm.FilterEq("p.repo_owner", f.Did),
		orm.FilterEq("p.repo_name", f.Name),
		orm.FilterEq("p.knot", f.Knot),
		orm.FilterIn("p.sha", shas),
	)
	if err != nil {
		l.Error("failed to fetch pipeline statuses", "err", err)
		// non-fatal
	}

	for _, p := range ps {
		m[p.Sha] = p
	}

	reactionMap, err := db.GetReactionMap(s.db, 20, pull.AtUri())
	if err != nil {
		l.Error("failed to get pull reactions", "err", err)
	}

	userReactions := map[models.ReactionKind]bool{}
	if user != nil {
		userReactions = db.GetReactionStatusMap(s.db, user.Active.Did, pull.AtUri())
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

	patch := pull.Submissions[roundIdInt].CombinedPatch()
	var diff types.DiffRenderer
	diff = patchutil.AsNiceDiff(patch, pull.TargetBranch)

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
	}

	s.pages.RepoSinglePull(w, pages.RepoSinglePullParams{
		LoggedInUser:       user,
		RepoInfo:           s.repoResolver.GetRepoInfo(r, user),
		Pull:               pull,
		Stack:              stack,
		Backlinks:          backlinks,
		BranchDeleteStatus: branchDeleteStatus,
		MergeCheck:         mergeCheckResponse,
		ResubmitCheck:      resubmitResult,
		Pipelines:          m,
		Diff:               diff,
		DiffOpts:           diffOpts,
		ActiveRound:        roundIdInt,
		IsInterdiff:        interdiff,

		Reactions:   reactionMap,
		UserReacted: userReactions,

		LabelDefs: defs,
	})
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

	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}
	host := fmt.Sprintf("%s://%s", scheme, f.Knot)

	xrpcc := indigoxrpc.Client{
		Host: host,
	}

	// combine patches of substack
	subStack := stack.Below(pull)
	// collect the portion of the stack that is mergeable
	mergeable := subStack.Mergeable()
	// combine each patch
	patch := mergeable.CombinedPatch()

	resp, err := tangled.RepoMergeCheck(
		r.Context(),
		&xrpcc,
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

	// convert xrpc response to internal types
	conflicts := make([]types.ConflictInfo, len(resp.Conflicts))
	for i, conflict := range resp.Conflicts {
		conflicts[i] = types.ConflictInfo{
			Filename: conflict.Filename,
			Reason:   conflict.Reason,
		}
	}

	result := types.MergeCheckResponse{
		IsConflicted: resp.Is_conflicted,
		Conflicts:    conflicts,
	}

	if resp.Message != nil {
		result.Message = *resp.Message
	}

	if resp.Error != nil {
		result.Error = *resp.Error
	}

	return result
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
	perms := s.enforcer.GetPermissionsInRepo(user.Active.Did, repo.Knot, repo.RepoIdentifier())
	if !slices.Contains(perms, "repo:push") {
		return nil
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	resp, err := tangled.GitTempGetBranch(r.Context(), xrpcc, branch, repo.RepoAt().String())
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

	var sourceRepo syntax.ATURI
	if pull.PullSource.RepoAt != nil {
		sourceRepo = *pull.PullSource.RepoAt
	} else {
		sourceRepo = repo.RepoAt()
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	branchResp, err := tangled.GitTempGetBranch(r.Context(), xrpcc, pull.PullSource.Branch, sourceRepo.String())
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

func (s *Pulls) RepoPulls(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "RepoPulls")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	params := r.URL.Query()
	page := pagination.FromContext(r.Context())

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	l = l.With("repo_at", f.RepoAt().String())

	query := searchquery.Parse(params.Get("q"))

	var state *models.PullState
	if urlState := params.Get("state"); urlState != "" {
		switch urlState {
		case "open":
			state = ptrPullState(models.PullOpen)
		case "closed":
			state = ptrPullState(models.PullClosed)
		case "merged":
			state = ptrPullState(models.PullMerged)
		}
		query.Set("state", urlState)
	} else if queryState := query.Get("state"); queryState != nil {
		switch *queryState {
		case "open":
			state = ptrPullState(models.PullOpen)
		case "closed":
			state = ptrPullState(models.PullClosed)
		case "merged":
			state = ptrPullState(models.PullMerged)
		}
	} else if _, hasQ := params["q"]; !hasQ {
		state = ptrPullState(models.PullOpen)
		query.Set("state", "open")
	}

	resolve := func(ctx context.Context, ident string) (string, error) {
		id, err := s.idResolver.ResolveIdent(ctx, ident)
		if err != nil {
			return "", err
		}
		return id.DID.String(), nil
	}

	authorDid, negatedAuthorDids := searchquery.ResolveAuthor(r.Context(), query, resolve, l)

	labels := query.GetAll("label")
	negatedLabels := query.GetAllNegated("label")
	labelValues := query.GetDynamicTags()
	negatedLabelValues := query.GetNegatedDynamicTags()

	// resolve DID-format label values: if a dynamic tag's label
	// definition has format "did", resolve the handle to a DID
	if len(labelValues) > 0 || len(negatedLabelValues) > 0 {
		labelDefs, err := db.GetLabelDefinitions(
			s.db,
			orm.FilterIn("at_uri", f.Labels),
			orm.FilterContains("scope", tangled.RepoPullNSID),
		)
		if err == nil {
			didLabels := make(map[string]bool)
			for _, def := range labelDefs {
				if def.ValueType.Format == models.ValueTypeFormatDid {
					didLabels[def.Name] = true
				}
			}
			labelValues = searchquery.ResolveDIDLabelValues(r.Context(), labelValues, didLabels, resolve, l)
			negatedLabelValues = searchquery.ResolveDIDLabelValues(r.Context(), negatedLabelValues, didLabels, resolve, l)
		} else {
			l.Debug("failed to fetch label definitions for DID resolution", "err", err)
		}
	}

	tf := searchquery.ExtractTextFilters(query)

	searchOpts := models.PullSearchOptions{
		Keywords:           tf.Keywords,
		Phrases:            tf.Phrases,
		RepoAt:             f.RepoAt().String(),
		State:              state,
		AuthorDid:          authorDid,
		Labels:             labels,
		LabelValues:        labelValues,
		NegatedKeywords:    tf.NegatedKeywords,
		NegatedPhrases:     tf.NegatedPhrases,
		NegatedLabels:      negatedLabels,
		NegatedLabelValues: negatedLabelValues,
		NegatedAuthorDids:  negatedAuthorDids,
		Page:               page,
	}

	var totalPulls int
	if state == nil {
		totalPulls = f.RepoStats.PullCount.Open + f.RepoStats.PullCount.Merged + f.RepoStats.PullCount.Closed
	} else {
		switch *state {
		case models.PullOpen:
			totalPulls = f.RepoStats.PullCount.Open
		case models.PullMerged:
			totalPulls = f.RepoStats.PullCount.Merged
		case models.PullClosed:
			totalPulls = f.RepoStats.PullCount.Closed
		}
	}

	repoInfo := s.repoResolver.GetRepoInfo(r, user)

	var pulls []*models.Pull

	if searchOpts.HasSearchFilters() {
		res, err := s.indexer.Search(r.Context(), searchOpts)
		if err != nil {
			l.Error("failed to search for pulls", "err", err)
			return
		}
		totalPulls = int(res.Total)
		l.Debug("searched pulls with indexer", "count", len(res.Hits))

		// update tab counts to reflect filtered results
		countOpts := searchOpts
		countOpts.Page = pagination.Page{Limit: 1}
		for _, ps := range []models.PullState{models.PullOpen, models.PullMerged, models.PullClosed} {
			countOpts.State = &ps
			countRes, err := s.indexer.Search(r.Context(), countOpts)
			if err != nil {
				continue
			}
			switch ps {
			case models.PullOpen:
				repoInfo.Stats.PullCount.Open = int(countRes.Total)
			case models.PullMerged:
				repoInfo.Stats.PullCount.Merged = int(countRes.Total)
			case models.PullClosed:
				repoInfo.Stats.PullCount.Closed = int(countRes.Total)
			}
		}

		if len(res.Hits) > 0 {
			pulls, err = db.GetPulls(
				s.db,
				orm.FilterIn("id", res.Hits),
			)
			if err != nil {
				l.Error("failed to get pulls", "err", err)
				s.pages.Notice(w, "pulls", "Failed to load pulls. Try again later.")
				return
			}
		}
	} else {
		filters := []orm.Filter{
			orm.FilterEq("repo_at", f.RepoAt()),
		}
		if state != nil {
			filters = append(filters, orm.FilterEq("state", *state))
		}
		pulls, err = db.GetPullsPaginated(
			s.db,
			page,
			filters...,
		)
		if err != nil {
			l.Error("failed to get pulls", "err", err)
			s.pages.Notice(w, "pulls", "Failed to load pulls. Try again later.")
			return
		}
	}

	for _, p := range pulls {
		var pullSourceRepo *models.Repo
		if p.PullSource != nil {
			if p.PullSource.RepoAt != nil {
				pullSourceRepo, err = db.GetRepoByAtUri(s.db, p.PullSource.RepoAt.String())
				if err != nil {
					l.Error("failed to get repo by at uri", "err", err, "repo_at", p.PullSource.RepoAt.String())
					continue
				} else {
					p.PullSource.Repo = pullSourceRepo
				}
			}
		}
	}

	var stacks []models.Stack
	var shas []string

	pullMap := make(map[string]*models.Pull)
	for _, p := range pulls {
		shas = append(shas, p.LatestSha())
		pullMap[p.AtUri().String()] = p
	}

	// track which PRs have been added to stacks
	visited := make(map[string]bool)

	// group stacked PRs together using dependent_on relationships
	for _, p := range pulls {
		if visited[p.AtUri().String()] {
			continue
		}

		root := p
		for root.DependentOn != nil {
			if parent, ok := pullMap[root.DependentOn.String()]; ok {
				root = parent
			} else {
				break // parent not in current page
			}
		}

		var stack models.Stack
		current := root
		for {
			if visited[current.AtUri().String()] {
				break
			}
			stack = append(stack, current)
			visited[current.AtUri().String()] = true

			found := false
			for _, candidate := range pulls {
				if candidate.DependentOn != nil &&
					candidate.DependentOn.String() == current.AtUri().String() {
					current = candidate
					found = true
					break
				}
			}
			if !found {
				break
			}
		}

		slices.Reverse(stack)
		stacks = append(stacks, stack)
	}

	ps, err := db.GetPipelineStatuses(
		s.db,
		len(shas),
		orm.FilterEq("p.repo_owner", f.Did),
		orm.FilterEq("p.repo_name", f.Name),
		orm.FilterEq("p.knot", f.Knot),
		orm.FilterIn("p.sha", shas),
	)
	if err != nil {
		l.Warn("failed to fetch pipeline statuses", "err", err)
		// non-fatal
	}
	m := make(map[string]models.Pipeline)
	for _, p := range ps {
		m[p.Sha] = p
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

	filterState := ""
	if state != nil {
		filterState = state.String()
	}

	s.pages.RepoPulls(w, pages.RepoPullsParams{
		LoggedInUser: s.oauth.GetMultiAccountUser(r),
		RepoInfo:     repoInfo,
		Pulls:        pulls,
		LabelDefs:    defs,
		FilterState:  filterState,
		FilterQuery:  query.String(),
		Stacks:       stacks,
		Pipelines:    m,
		Page:         page,
		PullCount:    totalPulls,
	})
}

func (s *Pulls) PullComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullComment")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
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

	roundNumberStr := chi.URLParam(r, "round")
	roundNumber, err := strconv.Atoi(roundNumberStr)
	if err != nil || roundNumber >= len(pull.Submissions) {
		http.Error(w, "bad round id", http.StatusBadRequest)
		l.Error("failed to parse round id", "err", err, "round_number_str", roundNumberStr)
		return
	}

	switch r.Method {
	case http.MethodGet:
		s.pages.PullNewCommentFragment(w, pages.PullNewCommentParams{
			LoggedInUser: user,
			RepoInfo:     s.repoResolver.GetRepoInfo(r, user),
			Pull:         pull,
			RoundNumber:  roundNumber,
		})
		return
	case http.MethodPost:
		body := r.FormValue("body")
		if body == "" {
			s.pages.Notice(w, "pull", "Comment body is required")
			return
		}

		mentions, references := s.mentionsResolver.Resolve(r.Context(), body)

		// Start a transaction
		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start transaction", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}
		defer tx.Rollback()

		createdAt := time.Now().Format(time.RFC3339)

		client, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}
		atResp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoPullCommentNSID,
			Repo:       user.Active.Did,
			Rkey:       tid.TID(),
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.RepoPullComment{
					Pull:      pull.AtUri().String(),
					Body:      body,
					CreatedAt: createdAt,
				},
			},
		})
		if err != nil {
			l.Error("failed to create pull comment", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		comment := &models.PullComment{
			OwnerDid:     user.Active.Did,
			RepoAt:       f.RepoAt().String(),
			PullId:       pull.PullId,
			Body:         body,
			CommentAt:    atResp.Uri,
			SubmissionId: pull.Submissions[roundNumber].ID,
			Mentions:     mentions,
			References:   references,
		}

		// Create the pull comment in the database with the commentAt field
		commentId, err := db.NewPullComment(tx, comment)
		if err != nil {
			l.Error("failed to create pull comment in database", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		// Commit the transaction
		if err = tx.Commit(); err != nil {
			l.Error("failed to commit transaction", "err", err)
			s.pages.Notice(w, "pull-comment", "Failed to create comment.")
			return
		}

		s.notifier.NewPullComment(r.Context(), comment, mentions)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d#comment-%d", ownerSlashRepo, pull.PullId, commentId))
		return
	}
}

func (s *Pulls) NewPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "NewPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	l = l.With("repo_at", f.RepoAt().String())

	switch r.Method {
	case http.MethodGet:
		xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}

		xrpcBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, f.RepoAt().String())
		if err != nil {
			if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
				l.Error("failed to call XRPC repo.branches", "xrpcerr", xrpcerr, "err", err)
				s.pages.Error503(w)
				return
			}
			l.Error("failed to fetch branches", "err", err)
			return
		}

		var result types.RepoBranchesResponse
		if err := json.Unmarshal(xrpcBytes, &result); err != nil {
			l.Error("failed to decode XRPC response", "err", err)
			s.pages.Error503(w)
			return
		}

		// can be one of "patch", "branch" or "fork"
		strategy := r.URL.Query().Get("strategy")
		// ignored if strategy is "patch"
		sourceBranch := r.URL.Query().Get("sourceBranch")
		targetBranch := r.URL.Query().Get("targetBranch")

		s.pages.RepoNewPull(w, pages.RepoNewPullParams{
			LoggedInUser: user,
			RepoInfo:     s.repoResolver.GetRepoInfo(r, user),
			Branches:     result.Branches,
			Strategy:     strategy,
			SourceBranch: sourceBranch,
			TargetBranch: targetBranch,
			Title:        r.URL.Query().Get("title"),
			Body:         r.URL.Query().Get("body"),
		})

	case http.MethodPost:
		title := r.FormValue("title")
		body := r.FormValue("body")
		targetBranch := r.FormValue("targetBranch")
		fromFork := r.FormValue("fork")
		sourceBranch := r.FormValue("sourceBranch")
		patch := r.FormValue("patch")
		userDid := syntax.DID(user.Active.Did)

		if targetBranch == "" {
			s.pages.Notice(w, "pull", "Target branch is required.")
			return
		}

		// Determine PR type based on input parameters
		roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(userDid.String(), f.Knot, f.RepoIdentifier())}
		isPushAllowed := roles.IsPushAllowed()
		isBranchBased := isPushAllowed && sourceBranch != "" && fromFork == ""
		isForkBased := fromFork != "" && sourceBranch != ""
		isPatchBased := patch != "" && !isBranchBased && !isForkBased
		isStacked := r.FormValue("isStacked") == "on"

		if isPatchBased && !patchutil.IsFormatPatch(patch) {
			if title == "" {
				s.pages.Notice(w, "pull", "Title is required for git-diff patches.")
				return
			}
			sanitizer := markup.NewSanitizer()
			if st := strings.TrimSpace(sanitizer.SanitizeDescription(title)); (st) == "" {
				s.pages.Notice(w, "pull", "Title is empty after HTML sanitization")
				return
			}
		}

		// Validate we have at least one valid PR creation method
		if !isBranchBased && !isPatchBased && !isForkBased {
			s.pages.Notice(w, "pull", "Neither source branch nor patch supplied.")
			return
		}

		// Can't mix branch-based and patch-based approaches
		if isBranchBased && patch != "" {
			s.pages.Notice(w, "pull", "Cannot select both patch and source branch.")
			return
		}

		// us, err := knotclient.NewUnsignedClient(f.Knot, s.config.Core.Dev)
		// if err != nil {
		// 	log.Printf("failed to create unsigned client to %s: %v", f.Knot, err)
		// 	s.pages.Notice(w, "pull", "Failed to create a pull request. Try again later.")
		// 	return
		// }

		// TODO: make capabilities an xrpc call
		caps := struct {
			PullRequests struct {
				FormatPatch       bool
				BranchSubmissions bool
				ForkSubmissions   bool
				PatchSubmissions  bool
			}
		}{
			PullRequests: struct {
				FormatPatch       bool
				BranchSubmissions bool
				ForkSubmissions   bool
				PatchSubmissions  bool
			}{
				FormatPatch:       true,
				BranchSubmissions: true,
				ForkSubmissions:   true,
				PatchSubmissions:  true,
			},
		}

		// caps, err := us.Capabilities()
		// if err != nil {
		// 	log.Println("error fetching knot caps", f.Knot, err)
		// 	s.pages.Notice(w, "pull", "Failed to create a pull request. Try again later.")
		// 	return
		// }

		if !caps.PullRequests.FormatPatch {
			s.pages.Notice(w, "pull", "This knot doesn't support format-patch. Unfortunately, there is no fallback for now.")
			return
		}

		// Handle the PR creation based on the type
		if isBranchBased {
			if !caps.PullRequests.BranchSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support branch-based pull requests. Try another way?")
				return
			}
			s.handleBranchBasedPull(w, r, f, userDid, title, body, targetBranch, sourceBranch, isStacked)
		} else if isForkBased {
			if !caps.PullRequests.ForkSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support fork-based pull requests. Try another way?")
				return
			}
			s.handleForkBasedPull(w, r, f, userDid, fromFork, title, body, targetBranch, sourceBranch, isStacked)
		} else if isPatchBased {
			if !caps.PullRequests.PatchSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support patch-based pull requests. Send your patch over email.")
				return
			}
			s.handlePatchBasedPull(w, r, f, userDid, title, body, targetBranch, patch, isStacked)
		}
		return
	}
}

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
) {
	l := s.logger.With("handler", "handleBranchBasedPull", "user", userDid, "target_branch", targetBranch, "source_branch", sourceBranch, "is_stacked", isStacked)

	scheme := "http"
	if !s.config.Core.Dev {
		scheme = "https"
	}
	host := fmt.Sprintf("%s://%s", scheme, repo.Knot)
	xrpcc := &indigoxrpc.Client{
		Host: host,
	}

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

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, combined, sourceRev, pullSource, isStacked)
}

func (s *Pulls) handlePatchBasedPull(w http.ResponseWriter, r *http.Request, repo *models.Repo, userDid syntax.DID, title, body, targetBranch, patch string, isStacked bool) {
	if err := s.validator.ValidatePatch(&patch); err != nil {
		s.logger.Error("patch validation failed", "err", err)
		s.pages.Notice(w, "pull", "Invalid patch format. Please provide a valid diff.")
		return
	}

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, "", "", nil, isStacked)
}

func (s *Pulls) handleForkBasedPull(w http.ResponseWriter, r *http.Request, repo *models.Repo, userDid syntax.DID, forkRepo string, title, body, targetBranch, sourceBranch string, isStacked bool) {
	l := s.logger.With("handler", "handleForkBasedPull", "user", userDid, "fork_repo", forkRepo, "target_branch", targetBranch, "source_branch", sourceBranch, "is_stacked", isStacked)

	repoString := strings.SplitN(forkRepo, "/", 2)
	forkOwnerDid := repoString[0]
	repoName := repoString[1]
	fork, err := db.GetForkByDid(s.db, forkOwnerDid, repoName)
	if errors.Is(err, sql.ErrNoRows) {
		s.pages.Notice(w, "pull", "No such fork.")
		return
	} else if err != nil {
		l.Error("failed to fetch fork", "err", err, "fork_owner_did", forkOwnerDid, "repo_name", repoName)
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
	forkScheme := "http"
	if !s.config.Core.Dev {
		forkScheme = "https"
	}
	forkHost := fmt.Sprintf("%s://%s", forkScheme, fork.Knot)
	forkXrpcc := &indigoxrpc.Client{
		Host: forkHost,
	}

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

	sourceRev := comparison.Rev2
	patch := comparison.FormatPatchRaw
	combined := comparison.CombinedPatchRaw

	if err := s.validator.ValidatePatch(&patch); err != nil {
		s.logger.Error("failed to validate patch", "err", err)
		s.pages.Notice(w, "pull", "Invalid patch format. Please provide a valid diff.")
		return
	}

	forkAtUri := fork.RepoAt()
	var forkDid *syntax.DID
	if fork.RepoDid != "" {
		forkDid = new(syntax.DID)
		*forkDid = syntax.DID(fork.RepoDid)
	}

	pullSource := &models.PullSource{
		Branch:  sourceBranch,
		RepoAt:  &forkAtUri,
		RepoDid: forkDid,
	}

	s.createPullRequest(w, r, repo, userDid, title, body, targetBranch, patch, combined, sourceRev, pullSource, isStacked)
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
		RepoAt:       repo.RepoAt(),
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
	}

	record := pull.AsRecord()
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoPullNSID,
		Repo:       userDid.String(),
		Rkey:       rkey,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &record,
		},
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
	pullId, err := db.NextPullId(tx, repo.RepoAt())
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

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pullId))
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
) {
	l := s.logger.With("handler", "createStackedPullRequest", "user", userDid, "target_branch", targetBranch, "source_rev", sourceRev)

	// run some necessary checks for stacked-prs first

	//  must be branch or fork based
	if sourceRev == "" {
		l.Error("stacked PR from patch-based pull")
		s.pages.Notice(w, "pull", "Stacking is only supported on branch and fork based pull-requests.")
		return
	}

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
	stack, err := s.newStack(r.Context(), repo, userDid, targetBranch, pullSource, formatPatches, blobs)
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
				Value: &lexutil.LexiconTypeDecoder{
					Val: &record,
				},
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

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls", ownerSlashRepo))
}

func (s *Pulls) ValidatePatch(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ValidatePatch")

	_, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	patch := r.FormValue("patch")
	if patch == "" {
		s.pages.Notice(w, "patch-error", "Patch is required.")
		return
	}

	if err := s.validator.ValidatePatch(&patch); err != nil {
		l.Error("failed to validate patch", "err", err)
		s.pages.Notice(w, "patch-error", "Invalid patch format. Please provide a valid git diff or format-patch.")
		return
	}

	if patchutil.IsFormatPatch(patch) {
		s.pages.Notice(w, "patch-preview", "git-format-patch detected. Title and description are optional; if left out, they will be extracted from the first commit.")
	} else {
		s.pages.Notice(w, "patch-preview", "Regular git-diff detected. Please provide a title and description.")
	}
}

func (s *Pulls) PatchUploadFragment(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)

	s.pages.PullPatchUploadFragment(w, pages.PullPatchUploadParams{
		RepoInfo: s.repoResolver.GetRepoInfo(r, user),
	})
}

func (s *Pulls) CompareBranchesFragment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "CompareBranchesFragment")

	user := s.oauth.GetMultiAccountUser(r)
	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}

	xrpcBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, f.RepoAt().String())
	if err != nil {
		l.Error("failed to fetch branches", "err", err)
		s.pages.Error503(w)
		return
	}

	var result types.RepoBranchesResponse
	if err := json.Unmarshal(xrpcBytes, &result); err != nil {
		l.Error("failed to decode XRPC response", "err", err)
		s.pages.Error503(w)
		return
	}

	branches := result.Branches
	sort.Slice(branches, func(i int, j int) bool {
		return branches[i].Commit.Committer.When.After(branches[j].Commit.Committer.When)
	})

	withoutDefault := []types.Branch{}
	for _, b := range branches {
		if b.IsDefault {
			continue
		}
		withoutDefault = append(withoutDefault, b)
	}

	s.pages.PullCompareBranchesFragment(w, pages.PullCompareBranchesParams{
		RepoInfo: s.repoResolver.GetRepoInfo(r, user),
		Branches: withoutDefault,
	})
}

func (s *Pulls) CompareForksFragment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "CompareForksFragment")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	forks, err := db.GetForksByDid(s.db, user.Active.Did)
	if err != nil {
		l.Error("failed to get forks", "err", err)
		return
	}

	s.pages.PullCompareForkFragment(w, pages.PullCompareForkParams{
		RepoInfo: s.repoResolver.GetRepoInfo(r, user),
		Forks:    forks,
		Selected: r.URL.Query().Get("fork"),
	})
}

func (s *Pulls) CompareForksBranchesFragment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "CompareForksBranchesFragment")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}

	forkVal := r.URL.Query().Get("fork")
	repoString := strings.SplitN(forkVal, "/", 2)
	forkOwnerDid := repoString[0]
	forkName := repoString[1]
	// fork repo
	repo, err := db.GetRepo(
		s.db,
		orm.FilterEq("did", forkOwnerDid),
		orm.FilterEq("name", forkName),
	)
	if err != nil {
		l.Error("failed to get repo", "fork_owner_did", forkOwnerDid, "fork_name", forkName, "err", err)
		return
	}

	sourceXrpcBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, repo.RepoAt().String())
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.branches for source", "xrpcerr", xrpcerr, "err", err)
			s.pages.Error503(w)
			return
		}
		l.Error("failed to fetch source branches", "err", err)
		return
	}

	// Decode source branches
	var sourceBranches types.RepoBranchesResponse
	if err := json.Unmarshal(sourceXrpcBytes, &sourceBranches); err != nil {
		l.Error("failed to decode source branches XRPC response", "err", err)
		s.pages.Error503(w)
		return
	}

	targetXrpcBytes, err := tangled.GitTempListBranches(r.Context(), xrpcc, "", 0, f.RepoAt().String())
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.branches for target", "xrpcerr", xrpcerr, "err", err)
			s.pages.Error503(w)
			return
		}
		l.Error("failed to fetch target branches", "err", err)
		return
	}

	// Decode target branches
	var targetBranches types.RepoBranchesResponse
	if err := json.Unmarshal(targetXrpcBytes, &targetBranches); err != nil {
		l.Error("failed to decode target branches XRPC response", "err", err)
		s.pages.Error503(w)
		return
	}

	sort.Slice(sourceBranches.Branches, func(i int, j int) bool {
		return sourceBranches.Branches[i].Commit.Committer.When.After(sourceBranches.Branches[j].Commit.Committer.When)
	})

	s.pages.PullCompareForkBranchesFragment(w, pages.PullCompareForkBranchesParams{
		RepoInfo:       s.repoResolver.GetRepoInfo(r, user),
		SourceBranches: sourceBranches.Branches,
		TargetBranches: targetBranches.Branches,
	})
}

func (s *Pulls) ResubmitPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ResubmitPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
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
		l = l.With("user", user.Active.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	if user == nil || user.Active.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Active.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	patch := r.FormValue("patch")

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Active.Did), pull, patch, "", "")
}

func (s *Pulls) resubmitBranch(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "resubmitBranch")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "resubmit-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "target_branch", pull.TargetBranch)

	if user == nil || user.Active.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Active.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Active.Did, f.Knot, f.RepoIdentifier())}
	if !roles.IsPushAllowed() {
		l.Warn("unauthorized user - no push permission")
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	scheme := "http"
	if !s.config.Core.Dev {
		scheme = "https"
	}
	host := fmt.Sprintf("%s://%s", scheme, f.Knot)
	xrpcc := &indigoxrpc.Client{
		Host: host,
	}

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

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Active.Did), pull, patch, combined, sourceRev)
}

func (s *Pulls) resubmitFork(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "resubmitFork")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "resubmit-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "target_branch", pull.TargetBranch)

	if user == nil || user.Active.Did != pull.OwnerDid {
		l.Warn("unauthorized user", "actual_user", user.Active.Did, "expected_owner", pull.OwnerDid)
		w.WriteHeader(http.StatusUnauthorized)
		return
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	forkRepo, err := db.GetRepoByAtUri(s.db, pull.PullSource.RepoAt.String())
	if err != nil {
		l.Error("failed to get source repo", "err", err, "repo_at", pull.PullSource.RepoAt.String())
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
	forkScheme := "http"
	if !s.config.Core.Dev {
		forkScheme = "https"
	}
	forkHost := fmt.Sprintf("%s://%s", forkScheme, forkRepo.Knot)
	forkXrpcBytes, err := tangled.RepoCompare(r.Context(), &indigoxrpc.Client{Host: forkHost}, forkRepo.RepoIdentifier(), hiddenRef, pull.PullSource.Branch)
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

	s.resubmitPullHelper(w, r, f, syntax.DID(user.Active.Did), pull, patch, combined, sourceRev)
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

	if err := s.validator.ValidatePatch(&patch); err != nil {
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
		Record: &lexutil.LexiconTypeDecoder{
			Val: &record,
		},
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

	newStack, err := s.newStack(r.Context(), repo, userDid, targetBranch, pull.PullSource, formatPatches, blobs)
	if err != nil {
		l.Error("failed to create resubmitted stack", "err", err)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
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

		err := db.AbandonPulls(tx, orm.FilterEq("repo_at", p.RepoAt), orm.FilterEq("at_uri", p.AtUri()))
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
				Value: &lexutil.LexiconTypeDecoder{
					Val: &record,
				},
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
				Value: &lexutil.LexiconTypeDecoder{
					Val: &record,
				},
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

func (s *Pulls) MergePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "MergePull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
		return
	}
	l = l.With("repo_at", f.RepoAt().String())

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-merge-error", "Failed to merge patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "target_branch", pull.TargetBranch)

	stack, ok := r.Context().Value("stack").(models.Stack)
	if !ok {
		l.Error("failed to get stack")
		s.pages.Notice(w, "pull-merge-error", "Failed to merge patch. Try again later.")
		return
	}

	// combine patches of substack
	subStack := stack.Below(pull)
	// collect the portion of the stack that is mergeable
	pullsToMerge := subStack.Mergeable()
	l = l.With("pulls_to_merge", len(pullsToMerge))

	patch := pullsToMerge.CombinedPatch()

	ident, err := s.idResolver.ResolveIdent(r.Context(), pull.OwnerDid)
	if err != nil {
		l.Error("failed to resolve identity", "err", err, "owner_did", pull.OwnerDid)
		w.WriteHeader(http.StatusNotFound)
		return
	}

	email, err := db.GetPrimaryEmail(s.db, pull.OwnerDid)
	if err != nil {
		l.Warn("failed to get primary email", "err", err, "owner_did", pull.OwnerDid)
	}

	authorName := ident.Handle.String()
	mergeInput := &tangled.RepoMerge_Input{
		Did:           f.Did,
		Name:          f.Name,
		Branch:        pull.TargetBranch,
		Patch:         patch,
		CommitMessage: &pull.Title,
		AuthorName:    &authorName,
	}

	if pull.Body != "" {
		mergeInput.CommitBody = &pull.Body
	}

	if email.Address != "" {
		mergeInput.AuthorEmail = &email.Address
	}

	client, err := s.oauth.ServiceClient(
		r,
		oauth.WithService(f.Knot),
		oauth.WithLxm(tangled.RepoMergeNSID),
		oauth.WithDev(s.config.Core.Dev),
	)
	if err != nil {
		l.Error("failed to connect to knot server", "err", err, "knot", f.Knot)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
		return
	}

	err = tangled.RepoMerge(r.Context(), client, mergeInput)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to merge", "xrpcerr", xrpcerr, "err", err)
		s.pages.Notice(w, "pull-merge-error", xrpcerr.Error())
		return
	}

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
		return
	}
	defer tx.Rollback()

	var atUris []syntax.ATURI
	for _, p := range pullsToMerge {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullMerged
	}
	err = db.MergePulls(tx, orm.FilterEq("repo_at", f.RepoAt()), orm.FilterIn("at_uri", atUris))
	if err != nil {
		l.Error("failed to update pull request status in database", "err", err)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
		return
	}

	err = tx.Commit()
	if err != nil {
		// TODO: this is unsound, we should also revert the merge from the knotserver here
		l.Error("failed to commit merge transaction", "err", err)
		s.pages.Notice(w, "pull-merge-error", "Failed to merge pull request. Try again later.")
		return
	}

	// notify about the pull merge
	for _, p := range pullsToMerge {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Active.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) ClosePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ClosePull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	// auth filter: only owner or collaborators can close
	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Active.Did, f.Knot, f.RepoIdentifier())}
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Active.Did == pull.OwnerDid
	isCloseAllowed := isOwner || isCollaborator || isPullAuthor
	if !isCloseAllowed {
		l.Error("unauthorized to close pull", "is_owner", isOwner, "is_collaborator", isCollaborator, "is_pull_author", isPullAuthor)
		s.pages.Notice(w, "pull-close", "You are unauthorized to close this pull.")
		return
	}

	// Start a transaction
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-close", "Failed to close pull.")
		return
	}
	defer tx.Rollback()

	// if this PR is stacked, then we want to close all PRs above this one on the stack
	stack := r.Context().Value("stack").(models.Stack)
	pullsToClose := stack.Above(pull)
	var atUris []syntax.ATURI
	for _, p := range pullsToClose {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullClosed
	}
	err = db.ClosePulls(
		tx,
		orm.FilterEq("repo_at", f.RepoAt()),
		orm.FilterIn("at_uri", atUris),
	)
	if err != nil {
		l.Error("failed to close pulls in database", "err", err, "pulls_to_close", len(pullsToClose))
		s.pages.Notice(w, "pull-close", "Failed to close pull.")
	}

	// Commit the transaction
	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, "pull-close", "Failed to close pull.")
		return
	}

	for _, p := range pullsToClose {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Active.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) ReopenPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ReopenPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Active.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		s.pages.Notice(w, "pull-reopen", "Failed to reopen pull.")
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "state", pull.State)

	// auth filter: only owner or collaborators can close
	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Active.Did, f.Knot, f.RepoIdentifier())}
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Active.Did == pull.OwnerDid
	isCloseAllowed := isOwner || isCollaborator || isPullAuthor
	if !isCloseAllowed {
		l.Error("unauthorized to reopen pull", "is_owner", isOwner, "is_collaborator", isCollaborator, "is_pull_author", isPullAuthor)
		s.pages.Notice(w, "pull-close", "You are unauthorized to close this pull.")
		return
	}

	// Start a transaction
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-reopen", "Failed to reopen pull.")
		return
	}
	defer tx.Rollback()

	// if this PR is stacked, then we want to reopen all PRs above this one on the stack
	stack := r.Context().Value("stack").(models.Stack)
	pullsToReopen := stack.Below(pull)
	var atUris []syntax.ATURI
	for _, p := range pullsToReopen {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullOpen
	}
	err = db.ReopenPulls(
		tx,
		orm.FilterEq("repo_at", f.RepoAt()),
		orm.FilterIn("at_uri", atUris),
	)
	if err != nil {
		l.Error("failed to reopen pulls in database", "err", err, "pulls_to_reopen", len(pullsToReopen))
		s.pages.Notice(w, "pull-close", "Failed to reopen pull.")
	}

	// Commit the transaction
	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, "pull-reopen", "Failed to reopen pull.")
		return
	}

	for _, p := range pullsToReopen {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Active.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) newStack(
	ctx context.Context,
	repo *models.Repo,
	userDid syntax.DID,
	targetBranch string,
	pullSource *models.PullSource,
	formatPatches []types.FormatPatch,
	blobs []*lexutil.LexBlob,
) (models.Stack, error) {
	var stack models.Stack
	var parentAtUri *syntax.ATURI
	for i, fp := range formatPatches {
		//  all patches must have a jj change-id
		_, err := fp.ChangeId()
		if err != nil {
			return nil, fmt.Errorf("Stacking is only supported if all patches contain a change-id commit header.")
		}

		title := fp.Title
		body := fp.Body
		rkey := tid.TID()

		mentions, references := s.mentionsResolver.Resolve(ctx, body)

		now := time.Now()

		pull := models.Pull{
			Title:        title,
			Body:         body,
			TargetBranch: targetBranch,
			OwnerDid:     userDid.String(),
			RepoAt:       repo.RepoAt(),
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

func gz(s string) io.Reader {
	var b bytes.Buffer
	w := gzip.NewWriter(&b)
	w.Write([]byte(s))
	w.Close()
	return &b
}

func ptrPullState(s models.PullState) *models.PullState { return &s }
