package state

import (
	"net/http"
	"sort"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/consts"
	"tangled.org/core/orm"
)

func (s *State) GoodFirstIssues(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "GoodFirstIssues")

	page := pagination.FromContext(r.Context())

	goodFirstIssueLabel := s.config.Label.GoodFirstIssue

	gfiLabelDef, err := db.GetLabelDefinition(s.db, orm.FilterEq("at_uri", goodFirstIssueLabel))
	if err != nil {
		l.Error("failed to get gfi label def", "err", err)
		s.pages.Error500(w)
		return
	}

	repoLabels, err := db.GetRepoLabels(s.db, orm.FilterEq("label_at", goodFirstIssueLabel))
	if err != nil {
		l.Error("failed to get repo labels", "err", err)
		s.pages.Error503(w)
		return
	}

	if len(repoLabels) == 0 {
		s.pages.GoodFirstIssues(w, pages.GoodFirstIssuesParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
			RepoGroups:   []*models.RepoGroup{},
			LabelDefs:    make(map[string]*models.LabelDefinition),
			Page:         page,
			GfiLabel:     gfiLabelDef,
		})
		return
	}

	repoUris := make([]string, 0, len(repoLabels))
	for _, rl := range repoLabels {
		repoUris = append(repoUris, string(rl.RepoDid))
	}

	allIssues, err := db.GetIssuesPaginated(
		s.db,
		pagination.Page{
			Limit: 500,
		},
		orm.FilterIn("repo_did", repoUris),
		orm.FilterEq("open", 1),
	)
	if err != nil {
		l.Error("failed to get issues", "err", err)
		s.pages.Error503(w)
		return
	}

	var goodFirstIssues []models.Issue
	for _, issue := range allIssues {
		if issue.Labels.ContainsLabel(goodFirstIssueLabel) {
			goodFirstIssues = append(goodFirstIssues, issue)
		}
	}

	repoGroups := make(map[string]*models.RepoGroup)
	for _, issue := range goodFirstIssues {
		if group, exists := repoGroups[issue.Repo.RepoDid]; exists {
			group.Issues = append(group.Issues, issue)
		} else {
			repoGroups[issue.Repo.RepoDid] = &models.RepoGroup{
				Repo:   issue.Repo,
				Issues: []models.Issue{issue},
			}
		}
	}

	var sortedGroups []*models.RepoGroup
	for _, group := range repoGroups {
		sortedGroups = append(sortedGroups, group)
	}

	sort.Slice(sortedGroups, func(i, j int) bool {
		iIsTangled := sortedGroups[i].Repo.Did == consts.TangledDid
		jIsTangled := sortedGroups[j].Repo.Did == consts.TangledDid

		// If one is tangled and the other isn't, non-tangled comes first
		if iIsTangled != jIsTangled {
			return jIsTangled // true if j is tangled (i should come first)
		}

		// Both tangled or both not tangled: sort by name
		return sortedGroups[i].Repo.Name < sortedGroups[j].Repo.Name
	})

	groupStart := page.Offset
	groupEnd := page.Offset + page.Limit
	if groupStart > len(sortedGroups) {
		groupStart = len(sortedGroups)
	}
	if groupEnd > len(sortedGroups) {
		groupEnd = len(sortedGroups)
	}

	paginatedGroups := sortedGroups[groupStart:groupEnd]

	var allIssuesFromGroups []models.Issue
	for _, group := range paginatedGroups {
		allIssuesFromGroups = append(allIssuesFromGroups, group.Issues...)
	}

	var allLabelDefs []models.LabelDefinition
	if len(allIssuesFromGroups) > 0 {
		labelDefUris := make(map[string]bool)
		for _, issue := range allIssuesFromGroups {
			for labelDefUri := range issue.Labels.Inner() {
				labelDefUris[labelDefUri] = true
			}
		}

		uriList := make([]string, 0, len(labelDefUris))
		for uri := range labelDefUris {
			uriList = append(uriList, uri)
		}

		if len(uriList) > 0 {
			allLabelDefs, err = db.GetLabelDefinitions(s.db, orm.FilterIn("at_uri", uriList))
			if err != nil {
				l.Error("failed to fetch labels", "err", err)
			}
		}
	}

	labelDefsMap := make(map[string]*models.LabelDefinition)
	for i := range allLabelDefs {
		labelDefsMap[allLabelDefs[i].AtUri().String()] = &allLabelDefs[i]
	}

	s.pages.GoodFirstIssues(w, pages.GoodFirstIssuesParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoGroups:   paginatedGroups,
		LabelDefs:    labelDefsMap,
		Page:         page,
		GfiLabel:     gfiLabelDef,
	})
}
