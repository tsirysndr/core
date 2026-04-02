package db

import (
	"database/sql"
	"errors"
	"fmt"
	"log"
	"slices"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func GetRepos(e Execer, filters ...orm.Filter) ([]models.Repo, error) {
	return GetReposPaginated(e, pagination.Page{}, filters...)
}

func GetReposPaginated(e Execer, page pagination.Page, filters ...orm.Filter) ([]models.Repo, error) {
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

	pageClause := ""
	if page.Limit != 0 {
		pageClause = fmt.Sprintf(" limit %d offset %d", page.Limit, page.Offset)
	}

	// main query to get repos with pagination
	query := fmt.Sprintf(`
		select
			id,
			did,
			name,
			knot,
			rkey,
			created,
			description,
			website,
			topics,
			source,
			spindle,
			repo_did
		from repos
		%s
		order by created desc
		%s
	`, whereClause, pageClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	repoMap := make(map[syntax.ATURI]*models.Repo)
	for rows.Next() {
		var repo models.Repo
		var createdAt string
		var description, website, topicStr, source, spindle, repoDid sql.NullString

		err := rows.Scan(
			&repo.Id,
			&repo.Did,
			&repo.Name,
			&repo.Knot,
			&repo.Rkey,
			&createdAt,
			&description,
			&website,
			&topicStr,
			&source,
			&spindle,
			&repoDid,
		)
		if err != nil {
			return nil, err
		}

		// parse created timestamp
		if t, err := time.Parse(time.RFC3339, createdAt); err == nil {
			repo.Created = t
		}

		// handle nullable fields
		if description.Valid {
			repo.Description = description.String
		}
		if website.Valid {
			repo.Website = website.String
		}
		if topicStr.Valid {
			repo.Topics = strings.Fields(topicStr.String)
		}
		if source.Valid {
			repo.Source = source.String
		}
		if spindle.Valid {
			repo.Spindle = spindle.String
		}
		if repoDid.Valid {
			repo.RepoDid = repoDid.String
		}

		repo.RepoStats = &models.RepoStats{}
		repoMap[repo.RepoAt()] = &repo
	}

	if err = rows.Err(); err != nil {
		return nil, err
	}

	// if no repos, return early
	if len(repoMap) == 0 {
		return nil, nil
	}

	// build IN clause for related queries
	inClause := strings.TrimSuffix(strings.Repeat("?, ", len(repoMap)), ", ")
	args = make([]any, len(repoMap))
	i := 0
	for _, r := range repoMap {
		args[i] = r.RepoAt()
		i++
	}

	// get labels for all repos
	labelsQuery := fmt.Sprintf(
		`select repo_at, label_at from repo_labels where repo_at in (%s)`,
		inClause,
	)

	rows, err = e.Query(labelsQuery, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var repoat, labelat string
		if err := rows.Scan(&repoat, &labelat); err != nil {
			continue
		}
		if r, ok := repoMap[syntax.ATURI(repoat)]; ok {
			r.Labels = append(r.Labels, labelat)
		}
	}

	// get primary language for all repos
	languageQuery := fmt.Sprintf(`
		select repo_at, language
		from (
			select
				repo_at, language,
				row_number() over (
					partition by repo_at
					order by bytes desc
				) as rn
			from repo_languages
			where repo_at in (%s)
				and is_default_ref = 1
				and language <> ''
		)
		where rn = 1
	`, inClause)

	rows, err = e.Query(languageQuery, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute lang query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoat, lang string
		if err := rows.Scan(&repoat, &lang); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[syntax.ATURI(repoat)]; ok {
			r.RepoStats.Language = lang
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute lang query: %w", err)
	}

	// get star counts
	starCountQuery := fmt.Sprintf(
		`select subject_at, count(1) from stars where subject_at in (%s) group by subject_at`,
		inClause,
	)

	rows, err = e.Query(starCountQuery, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute star-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoat string
		var count int
		if err := rows.Scan(&repoat, &count); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[syntax.ATURI(repoat)]; ok {
			r.RepoStats.StarCount = count
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute star-count query: %w", err)
	}

	// get issue counts
	issueCountQuery := fmt.Sprintf(`
		select
			repo_at,
			count(case when open = 1 then 1 end) as open_count,
			count(case when open = 0 then 1 end) as closed_count
		from issues
		where repo_at in (%s)
		group by repo_at
	`, inClause)

	rows, err = e.Query(issueCountQuery, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute issue-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoat string
		var open, closed int
		if err := rows.Scan(&repoat, &open, &closed); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[syntax.ATURI(repoat)]; ok {
			r.RepoStats.IssueCount.Open = open
			r.RepoStats.IssueCount.Closed = closed
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute issue-count query: %w", err)
	}

	// get pull counts
	pullCountQuery := fmt.Sprintf(`
		select
			repo_at,
			count(case when state = ? then 1 end) as open_count,
			count(case when state = ? then 1 end) as merged_count,
			count(case when state = ? then 1 end) as closed_count,
			count(case when state = ? then 1 end) as deleted_count
		from pulls
		where repo_at in (%s)
		group by repo_at
	`, inClause)

	pullArgs := append([]any{
		models.PullOpen,
		models.PullMerged,
		models.PullClosed,
		models.PullDeleted,
	}, args...)

	rows, err = e.Query(pullCountQuery, pullArgs...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute pulls-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoat string
		var open, merged, closed, deleted int
		if err := rows.Scan(&repoat, &open, &merged, &closed, &deleted); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[syntax.ATURI(repoat)]; ok {
			r.RepoStats.PullCount.Open = open
			r.RepoStats.PullCount.Merged = merged
			r.RepoStats.PullCount.Closed = closed
			r.RepoStats.PullCount.Deleted = deleted
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute pulls-count query: %w", err)
	}

	var repos []models.Repo
	for _, r := range repoMap {
		repos = append(repos, *r)
	}

	// sort by created timestamp (desc)
	slices.SortFunc(repos, func(a, b models.Repo) int {
		if a.Created.After(b.Created) {
			return -1
		}
		return 1
	})

	return repos, nil
}

// helper to get exactly one repo
func GetRepo(e Execer, filters ...orm.Filter) (*models.Repo, error) {
	repos, err := GetReposPaginated(e, pagination.Page{Limit: 1}, filters...)
	if err != nil {
		return nil, err
	}

	if repos == nil {
		return nil, sql.ErrNoRows
	}

	if len(repos) != 1 {
		return nil, fmt.Errorf("too few rows returned")
	}

	return &repos[0], nil
}

func CountRepos(e Execer, filters ...orm.Filter) (int64, error) {
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

	repoQuery := fmt.Sprintf(`select count(1) from repos %s`, whereClause)
	var count int64
	err := e.QueryRow(repoQuery, args...).Scan(&count)

	if !errors.Is(err, sql.ErrNoRows) && err != nil {
		return 0, err
	}

	return count, nil
}

func GetRepoByAtUri(e Execer, atUri string) (*models.Repo, error) {
	var repo models.Repo
	var nullableDescription sql.NullString
	var nullableWebsite sql.NullString
	var nullableTopicStr sql.NullString
	var nullableRepoDid sql.NullString
	var nullableSource sql.NullString
	var nullableSpindle sql.NullString

	row := e.QueryRow(`select id, did, name, knot, created, rkey, description, website, topics, source, spindle, repo_did from repos where at_uri = ?`, atUri)

	var createdAt string
	if err := row.Scan(&repo.Id, &repo.Did, &repo.Name, &repo.Knot, &createdAt, &repo.Rkey, &nullableDescription, &nullableWebsite, &nullableTopicStr, &nullableSource, &nullableSpindle, &nullableRepoDid); err != nil {
		return nil, err
	}
	createdAtTime, _ := time.Parse(time.RFC3339, createdAt)
	repo.Created = createdAtTime

	if nullableDescription.Valid {
		repo.Description = nullableDescription.String
	}
	if nullableWebsite.Valid {
		repo.Website = nullableWebsite.String
	}
	if nullableTopicStr.Valid {
		repo.Topics = strings.Fields(nullableTopicStr.String)
	}
	if nullableSource.Valid {
		repo.Source = nullableSource.String
	}
	if nullableSpindle.Valid {
		repo.Spindle = nullableSpindle.String
	}
	if nullableRepoDid.Valid {
		repo.RepoDid = nullableRepoDid.String
	}

	return &repo, nil
}

func PutRepo(tx *sql.Tx, repo models.Repo) error {
	var repoDid *string
	if repo.RepoDid != "" {
		repoDid = &repo.RepoDid
	}
	_, err := tx.Exec(
		`update repos
		set knot = ?, description = ?, website = ?, topics = ?, repo_did = coalesce(?, repo_did)
		where did = ? and rkey = ?
		`,
		repo.Knot, repo.Description, repo.Website, repo.TopicStr(), repoDid, repo.Did, repo.Rkey,
	)
	return err
}

func AddRepo(tx *sql.Tx, repo *models.Repo) error {
	var repoDid *string
	if repo.RepoDid != "" {
		repoDid = &repo.RepoDid
	}
	_, err := tx.Exec(
		`insert into repos
		(did, name, knot, rkey, at_uri, description, website, topics, source, repo_did)
		values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		repo.Did, repo.Name, repo.Knot, repo.Rkey, repo.RepoAt().String(), repo.Description, repo.Website, repo.TopicStr(), repo.Source, repoDid,
	)
	if err != nil {
		return fmt.Errorf("failed to insert repo: %w", err)
	}

	for _, dl := range repo.Labels {
		if err := SubscribeLabel(tx, &models.RepoLabel{
			RepoAt:  repo.RepoAt(),
			LabelAt: syntax.ATURI(dl),
		}); err != nil {
			return fmt.Errorf("failed to subscribe to label: %w", err)
		}
	}

	return nil
}

func RemoveRepo(e Execer, did, name string) error {
	_, err := e.Exec(`delete from repos where did = ? and name = ?`, did, name)
	return err
}

func GetRepoSource(e Execer, repoAt syntax.ATURI) (string, error) {
	var nullableSource sql.NullString
	err := e.QueryRow(`select source from repos where at_uri = ?`, repoAt).Scan(&nullableSource)
	if err != nil {
		return "", err
	}
	return nullableSource.String, nil
}

func GetRepoSourceRepo(e Execer, repoAt syntax.ATURI) (*models.Repo, error) {
	source, err := GetRepoSource(e, repoAt)
	if source == "" || errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if strings.HasPrefix(source, "did:") {
		return GetRepoByDid(e, source)
	}
	return GetRepoByAtUri(e, source)
}

func GetForksByDid(e Execer, did string) ([]models.Repo, error) {
	var repos []models.Repo

	rows, err := e.Query(
		`select distinct r.id, r.did, r.name, r.knot, r.rkey, r.description, r.website, r.created, r.source, r.repo_did
		from repos r
		left join collaborators c on r.at_uri = c.repo_at
		where (r.did = ? or c.subject_did = ?)
			and r.source is not null
			and r.source != ''
		order by r.created desc`,
		did, did,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var repo models.Repo
		var createdAt string
		var nullableDescription sql.NullString
		var nullableWebsite sql.NullString
		var nullableSource sql.NullString
		var nullableRepoDid sql.NullString

		err := rows.Scan(&repo.Id, &repo.Did, &repo.Name, &repo.Knot, &repo.Rkey, &nullableDescription, &nullableWebsite, &createdAt, &nullableSource, &nullableRepoDid)
		if err != nil {
			return nil, err
		}

		if nullableDescription.Valid {
			repo.Description = nullableDescription.String
		}
		if nullableWebsite.Valid {
			repo.Website = nullableWebsite.String
		}

		if nullableSource.Valid {
			repo.Source = nullableSource.String
		}
		if nullableRepoDid.Valid {
			repo.RepoDid = nullableRepoDid.String
		}

		createdAtTime, err := time.Parse(time.RFC3339, createdAt)
		if err != nil {
			repo.Created = time.Now()
		} else {
			repo.Created = createdAtTime
		}

		repos = append(repos, repo)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return repos, nil
}

func GetForkByDid(e Execer, did string, name string) (*models.Repo, error) {
	var repo models.Repo
	var createdAt string
	var nullableDescription sql.NullString
	var nullableWebsite sql.NullString
	var nullableTopicStr sql.NullString
	var nullableSource sql.NullString
	var nullableRepoDid sql.NullString

	row := e.QueryRow(
		`select id, did, name, knot, rkey, description, website, topics, created, source, repo_did
		from repos
		where did = ? and name = ? and source is not null and source != ''`,
		did, name,
	)

	err := row.Scan(&repo.Id, &repo.Did, &repo.Name, &repo.Knot, &repo.Rkey, &nullableDescription, &nullableWebsite, &nullableTopicStr, &createdAt, &nullableSource, &nullableRepoDid)
	if err != nil {
		return nil, err
	}

	if nullableDescription.Valid {
		repo.Description = nullableDescription.String
	}

	if nullableWebsite.Valid {
		repo.Website = nullableWebsite.String
	}

	if nullableTopicStr.Valid {
		repo.Topics = strings.Fields(nullableTopicStr.String)
	}

	if nullableSource.Valid {
		repo.Source = nullableSource.String
	}
	if nullableRepoDid.Valid {
		repo.RepoDid = nullableRepoDid.String
	}

	createdAtTime, err := time.Parse(time.RFC3339, createdAt)
	if err != nil {
		repo.Created = time.Now()
	} else {
		repo.Created = createdAtTime
	}

	return &repo, nil
}

func GetRepoByDid(e Execer, repoDid string) (*models.Repo, error) {
	return GetRepo(e, orm.FilterEq("repo_did", repoDid))
}

func EnqueuePdsRewritesForRepo(tx *sql.Tx, repoDid, repoAtUri string) error {
	type record struct {
		userDidCol string
		table      string
		nsid       string
		fkCol      string
	}
	sources := []record{
		{"did", "repos", "sh.tangled.repo", "at_uri"},
		{"did", "issues", "sh.tangled.repo.issue", "repo_at"},
		{"owner_did", "pulls", "sh.tangled.repo.pull", "repo_at"},
		{"did", "collaborators", "sh.tangled.repo.collaborator", "repo_at"},
		{"did", "artifacts", "sh.tangled.repo.artifact", "repo_at"},
		{"did", "stars", "sh.tangled.feed.star", "subject_at"},
	}

	for _, src := range sources {
		rows, err := tx.Query(
			fmt.Sprintf(`SELECT %s, rkey FROM %s WHERE %s = ?`, src.userDidCol, src.table, src.fkCol),
			repoAtUri,
		)
		if err != nil {
			return fmt.Errorf("query %s for pds rewrites: %w", src.table, err)
		}

		var pairs []struct{ did, rkey string }
		for rows.Next() {
			var d, r string
			if scanErr := rows.Scan(&d, &r); scanErr != nil {
				rows.Close()
				return fmt.Errorf("scan %s for pds rewrites: %w", src.table, scanErr)
			}
			pairs = append(pairs, struct{ did, rkey string }{d, r})
		}
		rows.Close()
		if rowsErr := rows.Err(); rowsErr != nil {
			return fmt.Errorf("iterate %s for pds rewrites: %w", src.table, rowsErr)
		}

		for _, p := range pairs {
			if err := EnqueuePdsRewrite(tx, p.did, repoDid, src.nsid, p.rkey, repoAtUri); err != nil {
				return fmt.Errorf("enqueue pds rewrite for %s/%s: %w", src.table, p.rkey, err)
			}
		}
	}

	profileRows, err := tx.Query(
		`SELECT DISTINCT did FROM profile_pinned_repositories WHERE pin = ?`,
		repoAtUri,
	)
	if err != nil {
		return fmt.Errorf("query profile_pinned_repositories for pds rewrites: %w", err)
	}
	var profileDids []string
	for profileRows.Next() {
		var d string
		if scanErr := profileRows.Scan(&d); scanErr != nil {
			profileRows.Close()
			return fmt.Errorf("scan profile_pinned_repositories for pds rewrites: %w", scanErr)
		}
		profileDids = append(profileDids, d)
	}
	profileRows.Close()
	if profileRowsErr := profileRows.Err(); profileRowsErr != nil {
		return fmt.Errorf("iterate profile_pinned_repositories for pds rewrites: %w", profileRowsErr)
	}

	for _, d := range profileDids {
		if err := EnqueuePdsRewrite(tx, d, repoDid, "sh.tangled.actor.profile", "self", repoAtUri); err != nil {
			return fmt.Errorf("enqueue pds rewrite for profile/%s: %w", d, err)
		}
	}

	return nil
}

type PdsRewrite struct {
	Id         int
	RepoDid    string
	RecordNsid string
	RecordRkey string
	OldRepoAt  string
}

func GetPendingPdsRewrites(e Execer, userDid string) ([]PdsRewrite, error) {
	rows, err := e.Query(
		`SELECT id, repo_did, record_nsid, record_rkey, old_repo_at
		FROM pds_rewrite_status
		WHERE user_did = ? AND status = 'pending'`,
		userDid,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var rewrites []PdsRewrite
	for rows.Next() {
		var r PdsRewrite
		if err := rows.Scan(&r.Id, &r.RepoDid, &r.RecordNsid, &r.RecordRkey, &r.OldRepoAt); err != nil {
			return nil, err
		}
		rewrites = append(rewrites, r)
	}
	return rewrites, rows.Err()
}

func CompletePdsRewrite(e Execer, id int) error {
	_, err := e.Exec(
		`UPDATE pds_rewrite_status SET status = 'done', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?`,
		id,
	)
	return err
}

func EnqueuePdsRewrite(e Execer, userDid, repoDid, recordNsid, recordRkey, oldRepoAt string) error {
	_, err := e.Exec(
		`INSERT INTO pds_rewrite_status
			(user_did, repo_did, record_nsid, record_rkey, old_repo_at, status)
		VALUES (?, ?, ?, ?, ?, 'pending')
		ON CONFLICT(user_did, record_nsid, record_rkey) DO UPDATE SET
			status = 'pending',
			repo_did = excluded.repo_did,
			old_repo_at = excluded.old_repo_at,
			updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`,
		userDid, repoDid, recordNsid, recordRkey, oldRepoAt,
	)
	return err
}

func CascadeRepoDid(tx *sql.Tx, repoAtUri, repoDid string) error {
	_, err := tx.Exec(
		`UPDATE repos SET repo_did = ? WHERE at_uri = ?`,
		repoDid, repoAtUri,
	)
	if err != nil {
		return fmt.Errorf("cascade repo_did to repos: %w", err)
	}

	_, err = tx.Exec(
		`UPDATE repos SET source = ? WHERE source = ?`,
		repoDid, repoAtUri,
	)
	if err != nil {
		return fmt.Errorf("cascade repo_did to repos.source: %w", err)
	}

	return nil
}

func UpdateDescription(e Execer, repoAt, newDescription string) error {
	_, err := e.Exec(
		`update repos set description = ? where at_uri = ?`, newDescription, repoAt)
	return err
}

func UpdateSpindle(e Execer, repoAt string, spindle *string) error {
	_, err := e.Exec(
		`update repos set spindle = ? where at_uri = ?`, spindle, repoAt)
	return err
}

func SubscribeLabel(e Execer, rl *models.RepoLabel) error {
	query := `insert or ignore into repo_labels (repo_at, label_at) values (?, ?)`

	_, err := e.Exec(query, rl.RepoAt.String(), rl.LabelAt.String())
	return err
}

func UnsubscribeLabel(e Execer, filters ...orm.Filter) error {
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

	query := fmt.Sprintf(`delete from repo_labels %s`, whereClause)
	_, err := e.Exec(query, args...)
	return err
}

func GetRepoLabels(e Execer, filters ...orm.Filter) ([]models.RepoLabel, error) {
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

	query := fmt.Sprintf(`select id, repo_at, label_at from repo_labels %s`, whereClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var labels []models.RepoLabel
	for rows.Next() {
		var label models.RepoLabel

		err := rows.Scan(&label.Id, &label.RepoAt, &label.LabelAt)
		if err != nil {
			return nil, err
		}

		labels = append(labels, label)
	}

	if err = rows.Err(); err != nil {
		return nil, err
	}

	return labels, nil
}
