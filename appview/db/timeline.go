package db

import (
	"sort"

	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

// followingFilter compiles to `key in (select subject_did from follows ...)`,
// keeping the following-set check inside sqlite rather than materializing the
// followed dids into a huge placeholder list.
func followingFilter(key, loggedInUserDid string) orm.Filter {
	return orm.FilterInSubquery(key, "select subject_did from follows where did = ?", loggedInUserDid)
}

// TODO: this gathers heterogenous events from different sources and aggregates
// them in code; if we did this entirely in sql, we could order and limit and paginate easily
func MakeTimeline(e Execer, limit int, loggedInUserDid string, limitToUsersIsFollowing bool) ([]models.TimelineGroup, error) {
	var events []models.TimelineEvent

	// Fetch more events than we need to so that when we collapse each individual
	// event into groups, we can still be relatively confident that we will have
	// `limit` groups to fill the timeline with. Adjust multiplier as necessary.
	fetchLimit := limit * 2

	var followingOnly string
	if limitToUsersIsFollowing {
		followingOnly = loggedInUserDid
	}

	repos, err := getTimelineRepos(e, fetchLimit, loggedInUserDid, followingOnly)
	if err != nil {
		return nil, err
	}

	stars, err := getTimelineStars(e, fetchLimit, loggedInUserDid, followingOnly)
	if err != nil {
		return nil, err
	}

	follows, err := getTimelineFollows(e, fetchLimit, loggedInUserDid, followingOnly)
	if err != nil {
		return nil, err
	}

	events = append(events, repos...)
	events = append(events, stars...)
	events = append(events, follows...)

	sort.Slice(events, func(i, j int) bool {
		return events[i].EventAt.After(events[j].EventAt)
	})

	groups := collapseTimeline(events)
	if len(groups) > limit {
		groups = groups[:limit]
	}
	return groups, nil
}

// collapseTimeline merges consecutive events that share the same operation
// and target into one TimelineGroup (assumes events are sorted newest-first).
func collapseTimeline(events []models.TimelineEvent) []models.TimelineGroup {
	var groups []models.TimelineGroup
	i := 0
	for i < len(events) {
		group := models.TimelineGroup{Primary: events[i]}
		j := i + 1
		for j < len(events) && canCollapse(events[i], events[j]) {
			group.Others = append(group.Others, events[j])
			j++
		}
		groups = append(groups, group)
		i = j
	}
	return groups
}

// canCollapse reports whether two adjacent events in the timeline represent
// the same operation on the same target (repo starred or user followed).
func canCollapse(a, b models.TimelineEvent) bool {
	switch {
	case a.RepoStar != nil && b.RepoStar != nil:
		if a.RepoStar.Repo == nil || b.RepoStar.Repo == nil {
			return false
		}
		return a.RepoStar.Repo.RepoAt() == b.RepoStar.Repo.RepoAt()
	case a.Follow != nil && b.Follow != nil:
		return a.Follow.SubjectDid == b.Follow.SubjectDid
	default:
		return false
	}
}

func fetchStarStatuses(e Execer, loggedInUserDid string, repos []models.Repo) (map[string]bool, error) {
	if loggedInUserDid == "" {
		return nil, nil
	}

	var repoDids []string
	for _, r := range repos {
		repoDids = append(repoDids, r.RepoDid)
	}

	return GetStarStatuses(e, loggedInUserDid, repoDids)
}

func getRepoStarInfo(repo *models.Repo, starStatuses map[string]bool) (bool, int64) {
	var isStarred bool
	if starStatuses != nil {
		isStarred = starStatuses[repo.RepoDid]
	}

	var starCount int64
	if repo.RepoStats != nil {
		starCount = int64(repo.RepoStats.StarCount)
	}

	return isStarred, starCount
}

func getTimelineRepos(e Execer, limit int, loggedInUserDid string, followingOnly string) ([]models.TimelineEvent, error) {
	filters := make([]orm.Filter, 0)
	if followingOnly != "" {
		filters = append(filters, followingFilter("did", followingOnly))
	}

	repos, err := GetReposPaginated(e, pagination.Page{Limit: limit}, filters...)
	if err != nil {
		return nil, err
	}

	// fetch all source repos
	var args []string
	for _, r := range repos {
		if r.Source != "" {
			args = append(args, r.Source)
		}
	}

	var origRepos []models.Repo
	if args != nil {
		origRepos, err = GetRepos(e, orm.FilterIn("repo_did", args))
	}
	if err != nil {
		return nil, err
	}

	didToRepo := make(map[string]models.Repo)
	for _, r := range origRepos {
		didToRepo[r.RepoDid] = r
	}

	starStatuses, err := fetchStarStatuses(e, loggedInUserDid, repos)
	if err != nil {
		return nil, err
	}

	var events []models.TimelineEvent
	for _, r := range repos {
		var source *models.Repo
		if r.Source != "" {
			if origRepo, ok := didToRepo[r.Source]; ok {
				source = &origRepo
			}
		}

		isStarred, starCount := getRepoStarInfo(&r, starStatuses)

		events = append(events, models.TimelineEvent{
			Repo:      &r,
			EventAt:   r.Created,
			Source:    source,
			IsStarred: isStarred,
			StarCount: starCount,
		})
	}

	return events, nil
}

func getTimelineStars(e Execer, limit int, loggedInUserDid string, followingOnly string) ([]models.TimelineEvent, error) {
	filters := make([]orm.Filter, 0)
	if followingOnly != "" {
		filters = append(filters, followingFilter("did", followingOnly))
	}

	stars, err := GetRepoStars(e, pagination.Page{Limit: limit}, filters...)
	if err != nil {
		return nil, err
	}

	var repos []models.Repo
	for _, s := range stars {
		repos = append(repos, *s.Repo)
	}

	starStatuses, err := fetchStarStatuses(e, loggedInUserDid, repos)
	if err != nil {
		return nil, err
	}

	var events []models.TimelineEvent
	for _, s := range stars {
		isStarred, starCount := getRepoStarInfo(s.Repo, starStatuses)

		events = append(events, models.TimelineEvent{
			RepoStar:  &s,
			EventAt:   s.Created,
			IsStarred: isStarred,
			StarCount: starCount,
		})
	}

	return events, nil
}

func getTimelineFollows(e Execer, limit int, loggedInUserDid string, followingOnly string) ([]models.TimelineEvent, error) {
	filters := make([]orm.Filter, 0)
	if followingOnly != "" {
		filters = append(filters, followingFilter("did", followingOnly))
	}

	follows, err := GetFollows(e, limit, filters...)
	if err != nil {
		return nil, err
	}

	var subjects []string
	for _, f := range follows {
		subjects = append(subjects, f.SubjectDid)
	}

	if subjects == nil {
		return nil, nil
	}

	profiles, err := GetProfiles(e, orm.FilterIn("did", subjects))
	if err != nil {
		return nil, err
	}

	followStatMap, err := GetFollowerFollowingCounts(e, subjects)
	if err != nil {
		return nil, err
	}

	var followStatuses map[string]models.FollowStatus
	if loggedInUserDid != "" {
		followStatuses, err = GetFollowStatuses(e, loggedInUserDid, subjects)
		if err != nil {
			return nil, err
		}
	}

	var events []models.TimelineEvent
	for _, f := range follows {
		profile, _ := profiles[f.SubjectDid]
		followStatMap, _ := followStatMap[f.SubjectDid]

		followStatus := models.IsNotFollowing
		if followStatuses != nil {
			followStatus = followStatuses[f.SubjectDid]
		}

		events = append(events, models.TimelineEvent{
			Follow:       &f,
			Profile:      profile,
			FollowStats:  &followStatMap,
			FollowStatus: &followStatus,
			EventAt:      f.FollowedAt,
		})
	}

	return events, nil
}
