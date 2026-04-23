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
	"iter"
	"log/slog"
	"net/http"
	"net/url"
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
	"github.com/bluesky-social/indigo/atproto/atclient"
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

func (s *Pulls) knotClient(host string) *indigoxrpc.Client {
	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}
	return &indigoxrpc.Client{Host: fmt.Sprintf("%s://%s", scheme, host)}
}

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

		mergeCheckResponse := s.mergeCheck(r, f, pull, stack)
		branchDeleteStatus := s.branchDeleteStatus(r, f, pull)
		resubmitResult := pages.Unknown
		if user.Did == pull.OwnerDid {
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
	if user != nil && user.Did == pull.OwnerDid {
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
		userReactions = db.GetReactionStatusMap(s.db, user.Did, pull.AtUri())
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
	if user != nil {
		participants := pull.Participants()
		vouchRelationships, err = db.GetVouchRelationshipsBatch(s.db, syntax.DID(user.Did), participants)
		if err != nil {
			l.Error("failed to fetch vouch relationships", "err", err)
		}
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

	err = s.pages.RepoSinglePull(w, pages.RepoSinglePullParams{
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

		LabelDefs:          defs,
		VouchRelationships: vouchRelationships,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
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
	perms := s.enforcer.GetPermissionsInRepo(user.Did, repo.Knot, repo.RepoIdentifier())
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
		l = l.With("user", user.Did)
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

	vouchRelationships := make(map[syntax.DID]*models.VouchRelationship)
	if user != nil {
		dids := make([]syntax.DID, len(pulls))
		for i, p := range pulls {
			dids[i] = syntax.DID(p.OwnerDid)
		}
		vouchRelationships, err = db.GetVouchRelationshipsBatch(s.db, syntax.DID(user.Did), dids)
		if err != nil {
			l.Error("failed to fetch vouch relationships", "err", err)
		}
	}

	err = s.pages.RepoPulls(w, pages.RepoPullsParams{
		LoggedInUser:       s.oauth.GetMultiAccountUser(r),
		RepoInfo:           repoInfo,
		Pulls:              pulls,
		LabelDefs:          defs,
		FilterState:        filterState,
		FilterQuery:        query.String(),
		Stacks:             stacks,
		Pipelines:          m,
		Page:               page,
		PullCount:          totalPulls,
		VouchRelationships: vouchRelationships,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
}

func (s *Pulls) PullComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullComment")

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
			Repo:       user.Did,
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
			OwnerDid:     user.Did,
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
		l = l.With("user", user.Did)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}
	l = l.With("repo_at", f.RepoAt().String())

	switch r.Method {
	case http.MethodGet:
		params, err := s.composeParams(r, f)
		if err != nil {
			l.Error("failed to build compose params", "err", err)
			s.pages.Error503(w)
			return
		}
		s.pages.RepoNewPull(w, params)

	case http.MethodPost:
		title := r.FormValue("title")
		body := r.FormValue("body")
		targetBranch := r.FormValue("targetBranch")
		fromFork := r.FormValue("fork")
		sourceBranch := r.FormValue("sourceBranch")
		patch := r.FormValue("patch")
		userDid := syntax.DID(user.Did)

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
		isStacked := r.FormValue("mode") == "stack" && !isPatchBased

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

		if isBranchBased && sourceBranch == targetBranch {
			s.pages.Notice(w, "pull", "Source and target branch must be different.")
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

		stackTitles := parseBracketedForm(r.Form, "stackTitle")
		stackBodies := parseBracketedForm(r.Form, "stackBody")

		// Handle the PR creation based on the type
		if isBranchBased {
			if !caps.PullRequests.BranchSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support branch-based pull requests. Try another way?")
				return
			}
			s.handleBranchBasedPull(w, r, f, userDid, title, body, targetBranch, sourceBranch, isStacked, stackTitles, stackBodies)
		} else if isForkBased {
			if !caps.PullRequests.ForkSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support fork-based pull requests. Try another way?")
				return
			}
			s.handleForkBasedPull(w, r, f, userDid, fromFork, title, body, targetBranch, sourceBranch, isStacked, stackTitles, stackBodies)
		} else if isPatchBased {
			if !caps.PullRequests.PatchSubmissions {
				s.pages.Notice(w, "pull", "This knot doesn't support patch-based pull requests. Send your patch over email.")
				return
			}
			s.handlePatchBasedPull(w, r, f, userDid, title, body, targetBranch, patch, isStacked, stackTitles, stackBodies)
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

func (s *Pulls) handleForkBasedPull(w http.ResponseWriter, r *http.Request, repo *models.Repo, userDid syntax.DID, forkRepo string, title, body, targetBranch, sourceBranch string, isStacked bool, stackTitles, stackBodies map[string]string) {
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

	s.applyCreationLabels(r.Context(), client, userDid, []*models.Pull{pull}, r.Form, repo)

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

	s.applyCreationLabels(r.Context(), client, userDid, stack, r.Form, repo)

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, repo)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls", ownerSlashRepo))
}

func (s *Pulls) MarkdownPreview(w http.ResponseWriter, r *http.Request) {
	body := r.FormValue("body")
	s.pages.MarkdownPreviewFragment(w, body)
}

func (s *Pulls) RefreshCompose(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "RefreshCompose")

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		s.pages.Error503(w)
		return
	}

	params, err := s.composeParams(r, f)
	if err != nil {
		l.Error("failed to build compose params", "err", err)
		s.pages.Error503(w)
		return
	}
	w.Header().Set("HX-Replace-Url", composeCanonicalURL(params))
	s.pages.PullComposeHostFragment(w, params)
}

func composeCanonicalURL(params pages.RepoNewPullParams) string {
	base := fmt.Sprintf("/%s/pulls/new", params.RepoInfo.FullName())
	q := url.Values{}
	if params.IsStacked {
		q.Set("mode", "stack")
	}
	if params.Source != "" && params.Source != pages.SourceBranch {
		q.Set("source", string(params.Source))
	}
	if params.SourceBranch != "" {
		q.Set("sourceBranch", params.SourceBranch)
	}
	if params.TargetBranch != "" {
		q.Set("targetBranch", params.TargetBranch)
	}
	if params.Source == pages.SourceFork && params.Fork != "" {
		q.Set("fork", params.Fork)
	}
	if len(q) == 0 {
		return base
	}
	return base + "?" + q.Encode()
}

func (s *Pulls) composeParams(r *http.Request, repo *models.Repo) (pages.RepoNewPullParams, error) {
	l := s.logger.With("handler", "composeParams")
	user := s.oauth.GetMultiAccountUser(r)

	branches, err := s.listBranches(r.Context(), repo)
	if err != nil {
		return pages.RepoNewPullParams{}, err
	}

	var forks []models.Repo
	if user != nil {
		forks, err = db.GetForksByDid(s.db, user.Did)
		if err != nil {
			l.Warn("failed to list user forks", "err", err, "user", user.Did)
		}
	}

	repoInfo := s.repoResolver.GetRepoInfo(r, user)
	source, ok := pages.ParseSource(r.FormValue("source"))
	if !ok {
		source = pages.SourceBranch
		if !repoInfo.Roles.IsPushAllowed() {
			source = pages.SourceFork
		}
	}

	sourceBranch := r.FormValue("sourceBranch")
	targetBranch := r.FormValue("targetBranch")
	fork := r.FormValue("fork")
	patch := r.FormValue("patch")

	if source == pages.SourceFork && fork == "" && len(forks) == 1 {
		fork = fmt.Sprintf("%s/%s", forks[0].Did, forks[0].Name)
	}

	var forkBranches []types.Branch
	var forkBranchesErr error
	if source == pages.SourceFork && fork != "" {
		forkBranches, forkBranchesErr = s.listForkBranches(r.Context(), fork)
		if forkBranchesErr != nil {
			l.Warn("failed to list fork branches", "err", forkBranchesErr, "fork", fork)
		}
	}

	sourceBranchList := sourceBranchChoices(branches)
	targetBranch = defaultTargetBranch(branches, targetBranch)
	sourceBranch = defaultSourceBranch(source, sourceBranch, sourceBranchList, forkBranches)

	comparison, diff, prefetchErr := s.prefetchComparison(r, repo, source, fork, targetBranch, sourceBranch, patch)
	var prefillErr string
	if joined := errors.Join(prefetchErr, forkBranchesErr); joined != nil {
		prefillErr = joined.Error()
	}

	mergeCheck := s.composeMergeCheck(r.Context(), repo, targetBranch, comparison)

	refreshUrl := fmt.Sprintf("/%s/pulls/new/refresh", repoInfo.FullName())
	var diffOpts types.DiffOpts
	if r.FormValue("diff") == "split" {
		diffOpts.Split = true
	}
	diffOpts.RefreshUrl = refreshUrl
	diffOpts.Target = "#diff-area"

	labelDefs, err := s.pullLabelDefs(repo)
	if err != nil {
		l.Warn("failed to load label definitions", "err", err)
	}
	labelState := labelStateFromForm(r.Form, labelDefs)
	perCidLabelForms := parseStackLabelForms(r.Form)
	stackLabelStates := make(map[string]models.LabelState, len(perCidLabelForms))
	for cid, perForm := range perCidLabelForms {
		stackLabelStates[cid] = labelStateFromForm(perForm, labelDefs)
	}

	stackTitles := parseBracketedForm(r.Form, "stackTitle")
	stackBodies := parseBracketedForm(r.Form, "stackBody")
	stackSplits := parseBracketedForm(r.Form, "stackSplit")

	title := r.FormValue("title")
	body := r.FormValue("body")
	if comparison != nil && len(comparison.FormatPatch) > 0 {
		first := comparison.FormatPatch[0]
		if title == "" && first.PatchHeader != nil {
			title = first.Title
		}
		if body == "" && first.PatchHeader != nil {
			body = first.Body
		}
	}

	isStacked := r.FormValue("mode") == "stack" && source != pages.SourcePatch
	var stackedDiffs []pages.StackedDiff
	if isStacked {
		stackedDiffs = stackPerCommitDiffs(comparison, targetBranch, refreshUrl, stackSplits)
	}

	return pages.RepoNewPullParams{
		LoggedInUser:     user,
		RepoInfo:         repoInfo,
		Branches:         branches,
		SourceBranches:   sourceBranchList,
		ForkBranches:     forkBranches,
		Forks:            forks,
		Source:           source,
		SourceBranch:     sourceBranch,
		TargetBranch:     targetBranch,
		Fork:             fork,
		Patch:            patch,
		Title:            title,
		Body:             body,
		IsStacked:        isStacked,
		Comparison:       comparison,
		Diff:             diff,
		DiffOpts:         diffOpts,
		StackedDiffs:     stackedDiffs,
		MergeCheck:       mergeCheck,
		StackTitles:      stackTitles,
		StackBodies:      stackBodies,
		PrefillError:     prefillErr,
		LabelDefs:        labelDefs,
		LabelState:       labelState,
		StackLabelStates: stackLabelStates,
	}, nil
}

func (s *Pulls) pullLabelDefs(repo *models.Repo) (map[string]*models.LabelDefinition, error) {
	defs, err := db.GetLabelDefinitions(
		s.db,
		orm.FilterIn("at_uri", repo.Labels),
		orm.FilterContains("scope", tangled.RepoPullNSID),
	)
	if err != nil {
		return nil, err
	}

	out := make(map[string]*models.LabelDefinition, len(defs))
	for i := range defs {
		d := defs[i]
		if !slices.Contains(d.Scope, tangled.RepoPullNSID) {
			continue
		}
		out[d.AtUri().String()] = &d
	}
	return out, nil
}

func formLabelEntries(form url.Values, defs map[string]*models.LabelDefinition) iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for key := range defs {
			for _, v := range form[key] {
				if v == "" {
					continue
				}
				if !yield(key, v) {
					return
				}
			}
		}
	}
}

func labelStateFromForm(form url.Values, defs map[string]*models.LabelDefinition) models.LabelState {
	state := models.NewLabelState()
	actx := &models.LabelApplicationCtx{Defs: defs}
	for key, val := range formLabelEntries(form, defs) {
		_ = actx.ApplyLabelOp(state, models.LabelOp{
			Operation:    models.LabelOperationAdd,
			OperandKey:   key,
			OperandValue: val,
		})
	}
	return state
}

func buildCreationLabelOps(
	userDid syntax.DID,
	subject syntax.ATURI,
	rkey string,
	form url.Values,
	defs map[string]*models.LabelDefinition,
	performedAt time.Time,
) []models.LabelOp {
	var ops []models.LabelOp
	for key, val := range formLabelEntries(form, defs) {
		ops = append(ops, models.LabelOp{
			Did:          userDid.String(),
			Rkey:         rkey,
			Subject:      subject,
			Operation:    models.LabelOperationAdd,
			OperandKey:   key,
			OperandValue: val,
			PerformedAt:  performedAt,
		})
	}
	return ops
}

func (s *Pulls) applyCreationLabels(
	ctx context.Context,
	client *atclient.APIClient,
	userDid syntax.DID,
	pulls []*models.Pull,
	form url.Values,
	repo *models.Repo,
) {
	l := s.logger.With("handler", "applyCreationLabels", "user", userDid)

	defs, err := s.pullLabelDefs(repo)
	if err != nil {
		l.Warn("failed to fetch label defs", "err", err)
		return
	}
	if len(defs) == 0 {
		return
	}

	perCidForms := parseStackLabelForms(form)

	applyAll := form.Get("applyLabelsToAll") == "on"
	var firstStackForm url.Values
	if applyAll && len(pulls) > 0 && len(pulls[0].Submissions) > 0 {
		if firstCid := pulls[0].Submissions[0].ChangeId(); firstCid != "" {
			if f, ok := perCidForms[firstCid]; ok {
				firstStackForm = f
			}
		}
	}

	performedAt := time.Now()
	for _, pull := range pulls {
		labelForm := form
		if firstStackForm != nil {
			labelForm = firstStackForm
		} else if len(perCidForms) > 0 && len(pull.Submissions) > 0 {
			if cid := pull.Submissions[0].ChangeId(); cid != "" {
				if perForm, ok := perCidForms[cid]; ok {
					labelForm = perForm
				}
			}
		}
		rkey := tid.TID()
		raw := buildCreationLabelOps(userDid, pull.AtUri(), rkey, labelForm, defs, performedAt)

		valid := make([]models.LabelOp, 0, len(raw))
		for _, op := range raw {
			def := defs[op.OperandKey]
			if err := s.validator.ValidateLabelOp(def, repo, &op); err != nil {
				l.Warn("invalid label op", "err", err, "subject", op.Subject, "key", op.OperandKey)
				continue
			}
			valid = append(valid, op)
		}
		if len(valid) == 0 {
			continue
		}

		record := models.LabelOpsAsRecord(valid)
		if _, err := comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.LabelOpNSID,
			Repo:       userDid.String(),
			Rkey:       rkey,
			Record:     &lexutil.LexiconTypeDecoder{Val: &record},
		}); err != nil {
			l.Warn("failed to write label ops to PDS", "err", err, "subject", pull.AtUri())
			continue
		}

		if err := s.indexLabelOps(ctx, valid); err != nil {
			l.Warn("failed to index label ops", "err", err, "subject", pull.AtUri())
			if _, err := comatproto.RepoDeleteRecord(context.Background(), client, &comatproto.RepoDeleteRecord_Input{
				Collection: tangled.LabelOpNSID,
				Repo:       userDid.String(),
				Rkey:       rkey,
			}); err != nil {
				l.Warn("failed to rollback label ops record from PDS", "err", err, "subject", pull.AtUri())
			}
			continue
		}

		s.notifier.NewPullLabelOp(ctx, pull)
	}
}

func (s *Pulls) indexLabelOps(ctx context.Context, ops []models.LabelOp) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	for _, op := range ops {
		if _, err := db.AddLabelOp(tx, &op); err != nil {
			return err
		}
	}
	return tx.Commit()
}

func (s *Pulls) listBranches(ctx context.Context, repo *models.Repo) ([]types.Branch, error) {
	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	xrpcBytes, err := tangled.GitTempListBranches(ctx, xrpcc, "", 0, repo.RepoAt().String())
	if err != nil {
		return nil, err
	}
	var result types.RepoBranchesResponse
	if err := json.Unmarshal(xrpcBytes, &result); err != nil {
		return nil, err
	}
	return result.Branches, nil
}

func (s *Pulls) listForkBranches(ctx context.Context, forkIdent string) ([]types.Branch, error) {
	parts := strings.SplitN(forkIdent, "/", 2)
	if len(parts) != 2 {
		return nil, fmt.Errorf("invalid fork identifier: %s", forkIdent)
	}
	forkRepo, err := db.GetRepo(s.db, orm.FilterEq("did", parts[0]), orm.FilterEq("name", parts[1]))
	if err != nil {
		return nil, err
	}
	branches, err := s.listBranches(ctx, forkRepo)
	if err != nil {
		return nil, err
	}
	return sortBranchesByRecency(branches), nil
}

func sourceBranchChoices(branches []types.Branch) []types.Branch {
	withoutDefault := slices.DeleteFunc(slices.Clone(branches), func(b types.Branch) bool {
		return b.IsDefault
	})
	return sortBranchesByRecency(withoutDefault)
}

func defaultTargetBranch(branches []types.Branch, current string) string {
	if slices.ContainsFunc(branches, func(b types.Branch) bool { return b.Reference.Name == current }) {
		return current
	}
	if idx := slices.IndexFunc(branches, func(b types.Branch) bool { return b.IsDefault }); idx >= 0 {
		return branches[idx].Reference.Name
	}
	return ""
}

func defaultSourceBranch(source pages.Source, current string, branchChoices, forkBranches []types.Branch) string {
	var candidates []types.Branch
	switch source {
	case pages.SourceFork:
		candidates = forkBranches
	case pages.SourceBranch:
		candidates = branchChoices
	default:
		return current
	}
	if slices.ContainsFunc(candidates, func(b types.Branch) bool { return b.Reference.Name == current }) {
		return current
	}
	if len(candidates) == 0 {
		return ""
	}
	return candidates[0].Reference.Name
}

func sortBranchesByRecency(branches []types.Branch) []types.Branch {
	out := slices.Clone(branches)
	sort.SliceStable(out, func(i, j int) bool {
		if out[i].Commit == nil || out[j].Commit == nil {
			return out[i].Commit != nil
		}
		return out[i].Commit.Committer.When.After(out[j].Commit.Committer.When)
	})
	return out
}

func (s *Pulls) prefetchComparison(r *http.Request, repo *models.Repo, source pages.Source, fork, targetBranch, sourceBranch, patch string) (*types.RepoFormatPatchResponse, *types.NiceDiff, error) {
	var (
		comparison *types.RepoFormatPatchResponse
		err        error
	)
	switch source {
	case pages.SourcePatch:
		if strings.TrimSpace(patch) == "" {
			return nil, nil, nil
		}
		if verr := s.validator.ValidatePatch(&patch); verr != nil {
			return nil, nil, fmt.Errorf("invalid patch: paste a valid git diff or format-patch")
		}
		comparison = parsePastedPatch(patch)
	case pages.SourceBranch:
		if targetBranch == "" || sourceBranch == "" {
			return nil, nil, nil
		}
		comparison, err = s.fetchBranchComparison(r.Context(), repo, targetBranch, sourceBranch)
	case pages.SourceFork:
		if fork == "" || targetBranch == "" || sourceBranch == "" {
			return nil, nil, nil
		}
		comparison, err = s.fetchForkComparison(r, fork, targetBranch, sourceBranch)
	default:
		return nil, nil, nil
	}
	if err != nil {
		s.logger.With("handler", "prefetchComparison").Warn("failed to pre-fetch comparison", "err", err, "source", source)
		return nil, nil, err
	}

	return comparison, deriveDiff(comparison, targetBranch), nil
}

func (s *Pulls) composeMergeCheck(ctx context.Context, repo *models.Repo, targetBranch string, comparison *types.RepoFormatPatchResponse) *types.MergeCheckResponse {
	if comparison == nil || targetBranch == "" {
		return nil
	}
	patch := comparison.CombinedPatchRaw
	if patch == "" {
		patch = comparison.FormatPatchRaw
	}
	if patch == "" {
		return nil
	}

	xrpcc := s.knotClient(repo.Knot)

	resp, err := tangled.RepoMergeCheck(ctx, xrpcc, &tangled.RepoMergeCheck_Input{
		Did:    repo.Did,
		Name:   repo.Name,
		Branch: targetBranch,
		Patch:  patch,
	})
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.With("handler", "composeMergeCheck").Warn("failed to check mergeability", "xrpcerr", xrpcerr, "err", err, "target_branch", targetBranch)
		return &types.MergeCheckResponse{Error: xrpcerr.Error()}
	}

	out := mergeCheckResponseFrom(resp)
	return &out
}

func bracketComponents(key, prefix string) ([]string, bool) {
	if !strings.HasPrefix(key, prefix) {
		return nil, false
	}
	rest := key[len(prefix):]
	var parts []string
	for len(rest) > 0 {
		if !strings.HasPrefix(rest, "[") {
			return nil, false
		}
		end := strings.Index(rest, "]")
		if end <= 0 {
			return nil, false
		}
		parts = append(parts, rest[1:end])
		rest = rest[end+1:]
	}
	if len(parts) == 0 {
		return nil, false
	}
	return parts, true
}

func parseBracketedForm(form url.Values, prefix string) map[string]string {
	out := make(map[string]string)
	for key, vals := range form {
		parts, ok := bracketComponents(key, prefix)
		if !ok || len(parts) != 1 || parts[0] == "" || len(vals) == 0 {
			continue
		}
		out[parts[0]] = vals[0]
	}
	return out
}

func parseStackLabelForms(form url.Values) map[string]url.Values {
	out := make(map[string]url.Values)
	for key, vals := range form {
		parts, ok := bracketComponents(key, "stackLabel")
		if !ok || len(parts) != 2 || parts[0] == "" || parts[1] == "" {
			continue
		}
		cid, atUri := parts[0], parts[1]
		if _, ok := out[cid]; !ok {
			out[cid] = make(url.Values)
		}
		out[cid][atUri] = append(out[cid][atUri], vals...)
	}
	return out
}

func parsePastedPatch(patch string) *types.RepoFormatPatchResponse {
	if patch == "" {
		return nil
	}
	response := &types.RepoFormatPatchResponse{FormatPatchRaw: patch}
	if patchutil.IsFormatPatch(patch) {
		if patches, err := patchutil.ExtractPatches(patch); err == nil {
			response.FormatPatch = patches
		}
	}
	return response
}

func (s *Pulls) fetchBranchComparison(ctx context.Context, repo *models.Repo, targetBranch, sourceBranch string) (*types.RepoFormatPatchResponse, error) {
	xrpcc := s.knotClient(repo.Knot)

	xrpcBytes, err := tangled.RepoCompare(ctx, xrpcc, repo.RepoIdentifier(), targetBranch, sourceBranch)
	if err != nil {
		return nil, err
	}

	var comparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(xrpcBytes, &comparison); err != nil {
		return nil, err
	}
	return &comparison, nil
}

func (s *Pulls) fetchForkComparison(r *http.Request, forkIdent, targetBranch, sourceBranch string) (*types.RepoFormatPatchResponse, error) {
	parts := strings.SplitN(forkIdent, "/", 2)
	if len(parts) != 2 {
		return nil, fmt.Errorf("invalid fork identifier: %s", forkIdent)
	}
	fork, err := db.GetForkByDid(s.db, parts[0], parts[1])
	if err != nil {
		return nil, err
	}

	client, err := s.oauth.ServiceClient(
		r,
		oauth.WithService(fork.Knot),
		oauth.WithLxm(tangled.RepoHiddenRefNSID),
		oauth.WithDev(s.config.Core.Dev),
	)
	if err != nil {
		return nil, err
	}

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
		return nil, xrpcerr
	}
	if !resp.Success {
		if resp.Error != nil {
			return nil, fmt.Errorf("hidden ref failed: %s", *resp.Error)
		}
		return nil, fmt.Errorf("hidden ref failed")
	}

	hiddenRef := fmt.Sprintf("hidden/%s/%s", sourceBranch, targetBranch)
	forkXrpcc := s.knotClient(fork.Knot)

	forkXrpcBytes, err := tangled.RepoCompare(r.Context(), forkXrpcc, fork.RepoIdentifier(), hiddenRef, sourceBranch)
	if err != nil {
		return nil, err
	}

	var comparison types.RepoFormatPatchResponse
	if err := json.Unmarshal(forkXrpcBytes, &comparison); err != nil {
		return nil, err
	}
	return &comparison, nil
}

func stackPerCommitDiffs(
	comparison *types.RepoFormatPatchResponse,
	targetBranch, refreshUrl string,
	stackSplits map[string]string,
) []pages.StackedDiff {
	if comparison == nil {
		return nil
	}
	out := make([]pages.StackedDiff, len(comparison.FormatPatch))
	for i, p := range comparison.FormatPatch {
		nd := patchutil.AsNiceDiff(p.Raw, targetBranch)
		out[i].Diff = &nd
		cid := p.ChangeIdOrEmpty()
		if cid == "" {
			continue
		}
		out[i].Opts = types.DiffOpts{
			Split:      stackSplits[cid] == "split",
			RefreshUrl: refreshUrl,
			Target:     fmt.Sprintf("#stack-diff-%s", cid),
			Field:      fmt.Sprintf("stackSplit[%s]", cid),
		}
	}
	return out
}

func deriveDiff(comparison *types.RepoFormatPatchResponse, targetBranch string) *types.NiceDiff {
	if comparison == nil {
		return nil
	}
	raw := comparison.CombinedPatchRaw
	if raw == "" {
		raw = comparison.FormatPatchRaw
	}
	d := patchutil.AsNiceDiff(raw, targetBranch)
	return &d
}

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

	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Did, f.Knot, f.RepoIdentifier())}
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

	newStack, err := s.newStack(r.Context(), repo, userDid, targetBranch, pull.PullSource, formatPatches, blobs, nil, nil)
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
		l = l.With("user", user.Did)
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
		oauth.WithTimeout(time.Second*20), // merge is quite slow on large repos, like witchsky
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
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) ClosePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ClosePull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
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
	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Did, f.Knot, f.RepoIdentifier())}
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Did == pull.OwnerDid
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
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) ReopenPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ReopenPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
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
	roles := repoinfo.RolesInRepo{Roles: s.enforcer.GetPermissionsInRepo(user.Did, f.Knot, f.RepoIdentifier())}
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Did == pull.OwnerDid
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
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
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
