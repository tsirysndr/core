package db

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log"
	"slices"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func RenameRepo(tx *sql.Tx, did, oldRkey, newRkey, newName string) error {
	newAtURI := fmt.Sprintf("at://%s/sh.tangled.repo/%s", did, newRkey)

	res, err := tx.Exec(
		`update repos set rkey = ?, name = ?, at_uri = ? where did = ? and rkey = ?`,
		newRkey, newName, newAtURI, did, oldRkey,
	)
	if err != nil {
		return fmt.Errorf("update repos row: %w", err)
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return fmt.Errorf("no repo row found for did=%s rkey=%s", did, oldRkey)
	}

	if _, err := tx.Exec(
		`update pipelines set repo_name = ? where repo_owner = ? and repo_name = ?`,
		newRkey, did, oldRkey,
	); err != nil {
		return fmt.Errorf("rename pipelines.repo_name: %w", err)
	}

	return nil
}

func UpdateRepoDisplayName(e Execer, did, rkey, newName string) error {
	_, err := e.Exec(
		`update repos set name = ? where did = ? and rkey = ?`,
		newName, did, rkey,
	)
	return err
}

func RecordRepoRename(e Execer, ownerDid, oldRkey, repoDid string) error {
	_, err := e.Exec(
		`insert into repo_renames (owner_did, old_rkey, repo_did)
		 values (?, ?, ?)
		 on conflict(owner_did, old_rkey) do update set
		     repo_did = excluded.repo_did,
		     renamed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`,
		ownerDid, oldRkey, repoDid,
	)
	return err
}

func DeleteRepoRename(e Execer, ownerDid, oldRkey string) error {
	_, err := e.Exec(
		`delete from repo_renames where owner_did = ? and old_rkey = ?`,
		ownerDid, oldRkey,
	)
	return err
}

func LookupRepoRename(e Execer, ownerDid, oldRkey string) (*models.Repo, error) {
	var repoDid string
	err := e.QueryRow(
		`select repo_did from repo_renames where owner_did = ? and old_rkey = ?`,
		ownerDid, oldRkey,
	).Scan(&repoDid)
	if err != nil {
		return nil, err
	}

	repo, err := GetRepoByDid(e, repoDid)
	if err != nil {
		return nil, err
	}
	return repo, nil
}

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

	repoMap := make(map[string]*models.Repo)
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
		repoMap[repo.RepoDid] = &repo
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
		args[i] = r.RepoDid
		i++
	}

	// get labels for all repos
	labelsQuery := fmt.Sprintf(
		`select repo_did, label_at from repo_labels where repo_did in (%s)`,
		inClause,
	)

	rows, err = e.Query(labelsQuery, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var repoDid, labelat string
		if err := rows.Scan(&repoDid, &labelat); err != nil {
			continue
		}
		if r, ok := repoMap[repoDid]; ok {
			r.Labels = append(r.Labels, labelat)
		}
	}

	// get primary language for all repos
	languageQuery := fmt.Sprintf(`
		select repo_did, language
		from (
			select
				repo_did, language,
				row_number() over (
					partition by repo_did
					order by bytes desc
				) as rn
			from repo_languages
			where repo_did in (%s)
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
		var repoDid, lang string
		if err := rows.Scan(&repoDid, &lang); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[repoDid]; ok {
			r.RepoStats.Language = lang
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute lang query: %w", err)
	}

	// get star counts
	starCountQuery := fmt.Sprintf(
		`select subject, count(1) from stars where subject_type = 'repo' and subject in (%s) group by subject`,
		inClause,
	)

	rows, err = e.Query(starCountQuery, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute star-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoDid string
		var count int
		if err := rows.Scan(&repoDid, &count); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[repoDid]; ok {
			r.RepoStats.StarCount = count
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute star-count query: %w", err)
	}

	// get issue counts
	issueCountQuery := fmt.Sprintf(`
		select
			repo_did,
			count(case when open = 1 then 1 end) as open_count,
			count(case when open = 0 then 1 end) as closed_count
		from issues
		where repo_did in (%s)
		group by repo_did
	`, inClause)

	rows, err = e.Query(issueCountQuery, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute issue-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoDid string
		var open, closed int
		if err := rows.Scan(&repoDid, &open, &closed); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[repoDid]; ok {
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
			repo_did,
			count(case when state = ? then 1 end) as open_count,
			count(case when state = ? then 1 end) as merged_count,
			count(case when state = ? then 1 end) as closed_count,
			count(case when state = ? then 1 end) as deleted_count
		from pulls
		where repo_did in (%s)
		group by repo_did
	`, inClause)

	pullArgs := append([]any{
		models.PullOpen,
		models.PullMerged,
		models.PullClosed,
		models.PullAbandoned,
	}, args...)

	rows, err = e.Query(pullCountQuery, pullArgs...)
	if err != nil {
		return nil, fmt.Errorf("failed to execute pulls-count query: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var repoDid string
		var open, merged, closed, deleted int
		if err := rows.Scan(&repoDid, &open, &merged, &closed, &deleted); err != nil {
			log.Println("err", "err", err)
			continue
		}
		if r, ok := repoMap[repoDid]; ok {
			r.RepoStats.PullCount.Open = open
			r.RepoStats.PullCount.Merged = merged
			r.RepoStats.PullCount.Closed = closed
			r.RepoStats.PullCount.Deleted = deleted
		}
	}
	if err = rows.Err(); err != nil {
		return nil, fmt.Errorf("failed to execute pulls-count query: %w", err)
	}

	// get forks — only query repos with a non-empty repo_did, since source
	// stores the upstream's repo_did and an empty string would match all
	var forksArgs []any
	for _, r := range repoMap {
		if r.RepoDid != "" {
			forksArgs = append(forksArgs, r.RepoDid)
		}
	}

	if len(forksArgs) > 0 {
		forksInClause := strings.TrimSuffix(strings.Repeat("?, ", len(forksArgs)), ", ")

		forksCountQuery := fmt.Sprintf(
			`select source, count(1) from repos where source in (%s) group by source`,
			forksInClause,
		)

		rows, err = e.Query(forksCountQuery, forksArgs...)
		if err != nil {
			return nil, fmt.Errorf("failed to execute fork-count query: %w", err)
		}
		defer rows.Close()

		for rows.Next() {
			var repodid string
			var count int
			if err := rows.Scan(&repodid, &count); err != nil {
				log.Println("failed to scan fork count", "err", err)
				continue
			}

			if r, ok := repoMap[repodid]; ok {
				r.RepoStats.ForkCount = count
			}
		}
		if err = rows.Err(); err != nil {
			return nil, fmt.Errorf("failed to execute fork-count query: %w", err)
		}
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
		set name = ?, knot = ?, description = ?, website = ?, topics = ?, repo_did = coalesce(?, repo_did)
		where did = ? and rkey = ?
		`,
		repo.Name, repo.Knot, repo.Description, repo.Website, repo.TopicStr(), repoDid, repo.Did, repo.Rkey,
	)
	return err
}

func AddRepo(tx *sql.Tx, repo *models.Repo) error {
	var repoDid *string
	if repo.RepoDid != "" {
		repoDid = &repo.RepoDid
	}
	result, err := tx.Exec(
		`insert into repos
		(did, name, knot, rkey, at_uri, description, website, topics, source, repo_did)
		values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		repo.Did, repo.Name, repo.Knot, repo.Rkey, repo.RepoAt().String(), repo.Description, repo.Website, repo.TopicStr(), repo.Source, repoDid,
	)
	if err != nil {
		return fmt.Errorf("failed to insert repo: %w", err)
	}

	id, err := result.LastInsertId()
	if err != nil {
		return fmt.Errorf("failed to get last insert id: %w", err)
	}
	repo.Id = id

	for _, dl := range repo.Labels {
		if err := SubscribeLabel(tx, &models.RepoLabel{
			RepoDid: syntax.DID(repo.RepoDid),
			LabelAt: syntax.ATURI(dl),
		}); err != nil {
			return fmt.Errorf("failed to subscribe to label: %w", err)
		}
	}

	return nil
}

func RemoveRepo(e Execer, did, rkey string) error {
	_, err := e.Exec(`delete from repos where did = ? and rkey = ?`, did, rkey)
	return err
}

func RemoveReposByKnot(e Execer, knot string) error {
	_, err := e.Exec(`delete from repos where knot = ?`, knot)
	return err
}

func GetRepoSource(e Execer, repoDid string) (string, error) {
	var nullableSource sql.NullString
	err := e.QueryRow(`select source from repos where repo_did = ?`, repoDid).Scan(&nullableSource)
	if err != nil {
		return "", err
	}
	return nullableSource.String, nil
}

func GetRepoSourceRepo(e Execer, repoDid string) (*models.Repo, error) {
	source, err := GetRepoSource(e, repoDid)
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
		left join collaborators c on r.repo_did = c.repo_did
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

func GetRepoByDid(e Execer, repoDid string) (*models.Repo, error) {
	return GetRepo(e, orm.FilterEq("repo_did", repoDid))
}

func GetForkByRepoDid(e Execer, repoDid string) (*models.Repo, error) {
	return GetRepo(e, orm.FilterEq("repo_did", repoDid), orm.FilterNotEq("source", ""))
}

// TODO: just queue every legacy records regardless of target repo has a DID or not.
// doable after we have `repo_did` column in db for each tables.
func EnqueuePdsRewritesForRepo(tx *sql.Tx, repoDid, repoAtUri string) error {
	type record struct {
		userDidCol string
		table      string
		nsid       syntax.NSID
		fkCol      string
		fkVal      string
	}
	sources := []record{
		{"did", "repos", tangled.RepoNSID, "at_uri", repoAtUri},
		{"did", "issues", tangled.RepoIssueNSID, "repo_did", repoDid},
		{"owner_did", "pulls", tangled.RepoPullNSID, "repo_did", repoDid},
		{"did", "collaborators", tangled.RepoCollaboratorNSID, "repo_did", repoDid},
		{"did", "artifacts", tangled.RepoArchiveNSID, "repo_did", repoDid},
		{"did", "stars", tangled.FeedStarNSID, "subject", repoDid},
	}

	for _, src := range sources {
		rows, err := tx.Query(
			fmt.Sprintf(`SELECT %s, rkey FROM %s WHERE %s = ?`, src.userDidCol, src.table, src.fkCol),
			src.fkVal,
		)
		if err != nil {
			return fmt.Errorf("query %s for pds rewrites: %w", src.table, err)
		}

		var pairs []struct{ did, rkey string }
		for rows.Next() {
			var d string
			var r sql.NullString
			if scanErr := rows.Scan(&d, &r); scanErr != nil {
				rows.Close()
				return fmt.Errorf("scan %s for pds rewrites: %w", src.table, scanErr)
			}
			if !r.Valid {
				continue
			}
			pairs = append(pairs, struct{ did, rkey string }{d, r.String})
		}
		rows.Close()
		if rowsErr := rows.Err(); rowsErr != nil {
			return fmt.Errorf("iterate %s for pds rewrites: %w", src.table, rowsErr)
		}

		for _, p := range pairs {
			if err := EnqueuePdsRecordMigration(context.Background(), tx, "add-repo-did", syntax.DID(p.did), src.nsid, syntax.RecordKey(p.rkey)); err != nil {
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
		if err := EnqueuePdsRecordMigration(context.Background(), tx, "add-repo-did", syntax.DID(d), tangled.ActorProfileNSID, "self"); err != nil {
			return fmt.Errorf("enqueue pds rewrite for profile/%s: %w", d, err)
		}
	}

	return nil
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

func UpdateDescription(e Execer, repoDid, newDescription string) error {
	_, err := e.Exec(
		`update repos set description = ? where repo_did = ?`, newDescription, repoDid)
	return err
}

func UpdateSpindle(e Execer, repoDid string, spindle *string) error {
	_, err := e.Exec(
		`update repos set spindle = ? where repo_did = ?`, spindle, repoDid)
	return err
}

func SubscribeLabel(e Execer, rl *models.RepoLabel) error {
	query := `insert or ignore into repo_labels (repo_did, label_at) values (?, ?)`

	_, err := e.Exec(query, string(rl.RepoDid), rl.LabelAt.String())
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

	query := fmt.Sprintf(`select id, repo_did, label_at from repo_labels %s`, whereClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var labels []models.RepoLabel
	for rows.Next() {
		var label models.RepoLabel

		err := rows.Scan(&label.Id, &label.RepoDid, &label.LabelAt)
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

func GetForkCount(e Execer, sourceDID string) (int, error) {
	forks := 0
	err := e.QueryRow(
		`select count(source) from repos where source = ?`, sourceDID).Scan(&forks)
	if err != nil {
		return 0, err
	}
	return forks, nil
}
