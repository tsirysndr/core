package pulls

import (
	"context"
	"net/http"
	"slices"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/searchquery"
	"tangled.org/core/orm"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/hostutil"
	"tangled.org/core/types"
)

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
		RepoDid:            f.RepoDid,
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
			orm.FilterEq("repo_did", f.RepoDid),
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
			if p.PullSource.RepoDid != nil {
				pullSourceRepo, err = db.GetRepoByDid(s.db, string(*p.PullSource.RepoDid))
				if err != nil {
					l.Error("failed to get repo by did", "err", err, "repo_did", p.PullSource.RepoDid.String())
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

	// commitId -> latest pipeline
	pipelines := func(ctx context.Context, shas []string) map[string]types.Pipeline {
		m := make(map[string]types.Pipeline)
		if f.Spindle == "" {
			return m
		}
		spindleUrl, err := hostutil.EnsureHttpScheme(f.Spindle)
		if err != nil {
			l.Error("invalid spindle host", "host", f.Spindle, "err", err)
			return m
		}
		xrpcc := &indigoxrpc.Client{Host: spindleUrl}
		out, err := tangled.CiQueryPipelines(ctx, xrpcc, shas, "", 0, f.RepoDid)
		if err != nil {
			l.Error("failed to fetch pipelines", "err", err)
			return m
		}

		for _, pipeline := range out.Pipelines {
			if pipeline == nil {
				continue
			}
			m[pipeline.Commit] = types.Pipeline{CiPipeline: pipeline}
		}
		return m
	}(r.Context(), shas)

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
		BaseParams:         pages.BaseParamsFromContext(r.Context()),
		RepoInfo:           repoInfo,
		Pulls:              pulls,
		LabelDefs:          defs,
		FilterState:        filterState,
		FilterQuery:        query.String(),
		Stacks:             stacks,
		Pipelines:          pipelines,
		Page:               page,
		PullCount:          totalPulls,
		VouchRelationships: vouchRelationships,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
	}
}
