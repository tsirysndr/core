package db

import (
	"database/sql"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func UpsertStar(e Execer, rkey string, star models.Star) error {
	_, err := e.Exec(
		`insert into stars (did, rkey, subject_type, subject, created)
		values (?, ?, ?, ?, ?)
		on conflict(did, rkey) do update set
			subject_type = excluded.subject_type,
			subject      = excluded.subject,
			created      = excluded.created`,
		star.Did,
		rkey,
		string(star.SubjectType),
		star.Subject,
		star.Created.Format(time.RFC3339),
	)
	return err
}

func GetStars(e Execer, subject string, page pagination.Page) ([]models.Star, error) {
	query := `
	select did, subject_type, subject, created
	from deduped_stars
	where subject = ?
	order by created desc
	limit ? offset ?
    `
	rows, err := e.Query(query, subject, page.Limit, page.Offset)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var stars []models.Star
	for rows.Next() {
		var star models.Star
		var created string
		if err := rows.Scan(&star.Did, &star.SubjectType, &star.Subject, &created); err != nil {
			return nil, err
		}

		star.Created = time.Now()
		if t, err := time.Parse(time.RFC3339, created); err == nil {
			star.Created = t
		}
		stars = append(stars, star)
	}

	return stars, rows.Err()
}

// Remove all stars from given user to subject
func DeleteStars(tx *sql.Tx, did syntax.DID, subject string) ([]syntax.ATURI, error) {
	var deleted []syntax.ATURI
	rows, err := tx.Query(
		`delete from stars
		where did = ? and subject = ?
		returning at_uri`,
		did,
		subject,
	)
	if err != nil {
		return nil, fmt.Errorf("deleting stars: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var aturi syntax.ATURI
		if err := rows.Scan(&aturi); err != nil {
			return nil, fmt.Errorf("scanning at_uri: %w", err)
		}
		deleted = append(deleted, aturi)
	}

	return deleted, nil
}

// Remove a star
func DeleteStarByRkey(e Execer, did string, rkey string) error {
	_, err := e.Exec(`delete from stars where did = ? and rkey = ?`, did, rkey)
	return err
}

func GetStarCount(e Execer, subjectType models.StarSubjectType, subject string) (int, error) {
	stars := 0
	err := e.QueryRow(
		`select count(*) from deduped_stars where subject_type = ? and subject = ?`,
		string(subjectType), subject,
	).Scan(&stars)
	if err != nil {
		return 0, err
	}
	return stars, nil
}

// getStarStatuses returns a map of subjects to star status for a given user
// This is an internal helper function to avoid N+1 queries
func getStarStatuses(e Execer, userDid string, subjects []string) (map[string]bool, error) {
	if len(subjects) == 0 || userDid == "" {
		return make(map[string]bool), nil
	}

	placeholders := make([]string, len(subjects))
	args := make([]any, len(subjects)+1)
	args[0] = userDid

	for i, subj := range subjects {
		placeholders[i] = "?"
		args[i+1] = subj
	}

	query := fmt.Sprintf(`
		SELECT subject
		FROM stars
		WHERE did = ? AND subject IN (%s)
	`, strings.Join(placeholders, ","))

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	result := make(map[string]bool)
	// Initialize all subjects as not starred
	for _, subj := range subjects {
		result[subj] = false
	}

	// Mark starred subjects as true
	for rows.Next() {
		var subj string
		if err := rows.Scan(&subj); err != nil {
			return nil, err
		}
		result[subj] = true
	}

	return result, nil
}

func GetStarStatus(e Execer, userDid string, subject string) bool {
	statuses, err := getStarStatuses(e, userDid, []string{subject})
	if err != nil {
		return false
	}
	return statuses[subject]
}

// GetStarStatuses returns a map of subjects to star status for a given user
func GetStarStatuses(e Execer, userDid string, subjects []string) (map[string]bool, error) {
	return getStarStatuses(e, userDid, subjects)
}

// GetRepoStars return a list of stars each holding target repository.
// If there isn't known repo with starred at-uri, those stars will be ignored.
func GetRepoStars(e Execer, page pagination.Page, filters ...orm.Filter) ([]models.RepoStar, error) {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	conditions = append(conditions, "subject_type = 'repo'")

	whereClause := " where " + strings.Join(conditions, " and ")

	pageClause := ""
	if page.Limit != 0 {
		pageClause = fmt.Sprintf(" limit %d offset %d", page.Limit, page.Offset)
	}

	repoQuery := fmt.Sprintf(
		`select did, subject_type, subject, created
		from deduped_stars
		%s
		order by created desc
		%s`,
		whereClause,
		pageClause,
	)
	rows, err := e.Query(repoQuery, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	starMap := make(map[string][]models.Star)
	for rows.Next() {
		var star models.Star
		var created string
		err := rows.Scan(&star.Did, &star.SubjectType, &star.Subject, &created)
		if err != nil {
			return nil, err
		}

		star.Created = time.Now()
		if t, err := time.Parse(time.RFC3339, created); err == nil {
			star.Created = t
		}

		starMap[star.Subject] = append(starMap[star.Subject], star)
	}

	// populate *Repo in each star
	args = make([]any, len(starMap))
	i := 0
	for r := range starMap {
		args[i] = r
		i++
	}

	if len(args) == 0 {
		return nil, nil
	}

	repos, err := GetRepos(e, orm.FilterIn("repo_did", args))
	if err != nil {
		return nil, err
	}

	var repoStars []models.RepoStar
	for _, r := range repos {
		if stars, ok := starMap[r.RepoDid]; ok {
			for _, star := range stars {
				repoStars = append(repoStars, models.RepoStar{
					Star: star,
					Repo: &r,
				})
			}
		}
	}

	slices.SortFunc(repoStars, func(a, b models.RepoStar) int {
		if a.Created.After(b.Created) {
			return -1
		}
		if b.Created.After(a.Created) {
			return 1
		}
		return 0
	})

	return repoStars, nil
}

func CountStars(e Execer, filters ...orm.Filter) (int64, error) {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	repoQuery := fmt.Sprintf(`select count(*) from deduped_stars %s`, whereClause)
	var count int64
	if err := e.QueryRow(repoQuery, args...).Scan(&count); err != nil {
		return 0, err
	}

	return count, nil
}

// GetTopStarredReposLastWeek returns the top 8 most starred repositories from the last week
func GetTopStarredReposLastWeek(e Execer) ([]models.Repo, error) {
	// first, get the top repo DIDs by star count from the last week
	query := `
		select subject
		from deduped_stars
		where subject_type = 'repo'
		  and created >= datetime('now', '-7 days')
		group by subject
		order by count(*) desc
		limit 5
	`

	rows, err := e.Query(query)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var repoDids []string
	for rows.Next() {
		var repoDid string
		err := rows.Scan(&repoDid)
		if err != nil {
			return nil, err
		}
		repoDids = append(repoDids, repoDid)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	if len(repoDids) == 0 {
		return []models.Repo{}, nil
	}

	// get full repo data
	repos, err := GetRepos(e, orm.FilterIn("repo_did", repoDids))
	if err != nil {
		return nil, err
	}

	// sort repos by the original trending order
	repoMap := make(map[string]models.Repo)
	for _, repo := range repos {
		repoMap[repo.RepoDid] = repo
	}

	orderedRepos := make([]models.Repo, 0, len(repoDids))
	for _, did := range repoDids {
		if repo, exists := repoMap[did]; exists {
			orderedRepos = append(orderedRepos, repo)
		}
	}

	return orderedRepos, nil
}
