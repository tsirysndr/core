package pulls

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"slices"
	"sort"
	"strings"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

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
		roles := s.acl.RolesInRepo(r.Context(), f, userDid.String())
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
	forks = slices.DeleteFunc(forks, func(f models.Repo) bool {
		return f.RepoDid == ""
	})

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
		fork = forks[0].RepoDid
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
		BaseParams: pages.BaseParamsFromContext(r.Context()),
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

func (s *Pulls) listBranches(ctx context.Context, repo *models.Repo) ([]types.Branch, error) {
	xrpcc := &indigoxrpc.Client{Host: s.config.KnotMirror.Url}
	xrpcBytes, err := tangled.GitTempListBranches(ctx, xrpcc, "", 0, repo.RepoDid)
	if err != nil {
		return nil, err
	}
	var result types.RepoBranchesResponse
	if err := json.Unmarshal(xrpcBytes, &result); err != nil {
		return nil, err
	}
	return result.Branches, nil
}

func (s *Pulls) listForkBranches(ctx context.Context, forkRepoDid string) ([]types.Branch, error) {
	if forkRepoDid == "" {
		return nil, fmt.Errorf("fork not found")
	}
	forkRepo, err := db.GetForkByRepoDid(s.db, forkRepoDid)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, fmt.Errorf("fork not found")
	}
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

func (s *Pulls) fetchForkComparison(r *http.Request, forkRepoDid, targetBranch, sourceBranch string) (*types.RepoFormatPatchResponse, error) {
	if forkRepoDid == "" {
		return nil, fmt.Errorf("fork not found")
	}
	fork, err := db.GetForkByRepoDid(s.db, forkRepoDid)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, fmt.Errorf("fork not found")
	}
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
