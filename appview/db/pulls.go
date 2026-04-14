package db

import (
	"cmp"
	"database/sql"
	"errors"
	"fmt"
	"maps"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/ipfs/go-cid"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
	"tangled.org/core/sets"
)

func comparePullSource(existing, new *models.PullSource) bool {
	if existing == nil && new == nil {
		return true
	}
	if existing == nil || new == nil {
		return false
	}
	if existing.Branch != new.Branch {
		return false
	}
	if existing.RepoDid == nil && new.RepoDid == nil {
		return true
	}
	if existing.RepoDid == nil || new.RepoDid == nil {
		return false
	}
	return *existing.RepoDid == *new.RepoDid
}

func compareSubmissions(existing, new []*models.PullSubmission) bool {
	if len(existing) != len(new) {
		return false
	}
	for i := range existing {
		if existing[i].Blob.Ref.String() != new[i].Blob.Ref.String() {
			return false
		}
		if existing[i].Blob.MimeType != new[i].Blob.MimeType {
			return false
		}
		if existing[i].Blob.Size != new[i].Blob.Size {
			return false
		}
	}
	return true
}

func PutPull(tx *sql.Tx, pull *models.Pull) error {
	// ensure sequence exists
	_, err := tx.Exec(`
		insert or ignore into repo_pull_seqs (repo_did, next_pull_id)
		values (?, 1)
		`, pull.RepoDid)
	if err != nil {
		return err
	}

	pulls, err := GetPulls(
		tx,
		orm.FilterEq("owner_did", pull.OwnerDid),
		orm.FilterEq("rkey", pull.Rkey),
	)
	switch {
	case err != nil:
		return err
	case len(pulls) == 0:
		return createNewPull(tx, pull)
	case len(pulls) != 1: // should be unreachable
		return fmt.Errorf("invalid number of pulls returned: %d", len(pulls))
	default:
		existingPull := pulls[0]
		if existingPull.State == models.PullMerged {
			return nil
		}

		dependentOnEqual := (existingPull.DependentOn == nil && pull.DependentOn == nil) ||
			(existingPull.DependentOn != nil && pull.DependentOn != nil && *existingPull.DependentOn == *pull.DependentOn)

		pullSourceEqual := comparePullSource(existingPull.PullSource, pull.PullSource)
		submissionsEqual := compareSubmissions(existingPull.Submissions, pull.Submissions)

		if existingPull.Title == pull.Title &&
			existingPull.Body == pull.Body &&
			existingPull.TargetBranch == pull.TargetBranch &&
			existingPull.RepoDid == pull.RepoDid &&
			dependentOnEqual &&
			pullSourceEqual &&
			submissionsEqual {
			return nil
		}

		isLonger := len(existingPull.Submissions) < len(pull.Submissions)
		if isLonger {
			isAppendOnly := compareSubmissions(existingPull.Submissions, pull.Submissions[:len(existingPull.Submissions)])
			if !isAppendOnly {
				return fmt.Errorf("the new pull does not treat submissions as append-only")
			}
		} else if !submissionsEqual {
			return fmt.Errorf("the new pull does not treat submissions as append-only")
		}

		pull.ID = existingPull.ID
		pull.PullId = existingPull.PullId
		return updatePull(tx, pull, existingPull)
	}
}

func createNewPull(tx *sql.Tx, pull *models.Pull) error {
	_, err := tx.Exec(`
		insert or ignore into repo_pull_seqs (repo_did, next_pull_id)
		values (?, 1)
		`, pull.RepoDid)
	if err != nil {
		return err
	}

	var nextId int
	err = tx.QueryRow(`
		update repo_pull_seqs
		set next_pull_id = next_pull_id + 1
		where repo_did = ?
		returning next_pull_id - 1
		`, pull.RepoDid).Scan(&nextId)
	if err != nil {
		return err
	}

	pull.PullId = nextId
	pull.State = models.PullOpen

	var sourceBranch, sourceRepoDid *string
	if pull.PullSource != nil {
		sourceBranch = &pull.PullSource.Branch
		if pull.PullSource.RepoDid != nil {
			x := string(*pull.PullSource.RepoDid)
			sourceRepoDid = &x
		}
	}

	result, err := tx.Exec(
		`
		insert into pulls (
			repo_did,
			owner_did,
			pull_id,
			title,
			target_branch,
			body,
			rkey,
			state,
			dependent_on,
			source_branch,
			source_repo_did
		)
		values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		pull.RepoDid,
		pull.OwnerDid,
		pull.PullId,
		pull.Title,
		pull.TargetBranch,
		pull.Body,
		pull.Rkey,
		pull.State,
		pull.DependentOn,
		sourceBranch,
		sourceRepoDid,
	)
	if err != nil {
		return err
	}

	// Set the database primary key ID
	id, err := result.LastInsertId()
	if err != nil {
		return err
	}
	pull.ID = int(id)

	for i, s := range pull.Submissions {
		_, err = tx.Exec(`
			insert into pull_submissions (
				pull_at,
				round_number,
				patch,
				combined,
				source_rev,
				patch_blob_ref,
				patch_blob_mime,
				patch_blob_size
			)
			values (?, ?, ?, ?, ?, ?, ?, ?)
		`,
			pull.AtUri(),
			i,
			s.Patch,
			s.Combined,
			s.SourceRev,
			s.Blob.Ref.String(),
			s.Blob.MimeType,
			s.Blob.Size,
		)
		if err != nil {
			return err
		}
	}

	if err := putReferences(tx, pull.AtUri(), pull.References); err != nil {
		return fmt.Errorf("put reference_links: %w", err)
	}

	return nil
}

func updatePull(tx *sql.Tx, pull *models.Pull, existingPull *models.Pull) error {
	var sourceBranch, sourceRepoDid *string
	if pull.PullSource != nil {
		sourceBranch = &pull.PullSource.Branch
		if pull.PullSource.RepoDid != nil {
			x := string(*pull.PullSource.RepoDid)
			sourceRepoDid = &x
		}
	}

	_, err := tx.Exec(`
		update pulls set
			title = ?,
			body = ?,
			target_branch = ?,
			dependent_on = ?,
			source_branch = ?,
			source_repo_did = ?
		where owner_did = ? and rkey = ?
	`, pull.Title, pull.Body, pull.TargetBranch, pull.DependentOn, sourceBranch, sourceRepoDid, pull.OwnerDid, pull.Rkey)
	if err != nil {
		return err
	}

	// insert new submissions (append-only)
	for i := len(existingPull.Submissions); i < len(pull.Submissions); i++ {
		s := pull.Submissions[i]
		_, err = tx.Exec(`
			insert into pull_submissions (
				pull_at,
				round_number,
				patch,
				combined,
				source_rev,
				patch_blob_ref,
				patch_blob_mime,
				patch_blob_size
			)
			values (?, ?, ?, ?, ?, ?, ?, ?)
		`,
			pull.AtUri(),
			i,
			s.Patch,
			s.Combined,
			s.SourceRev,
			s.Blob.Ref.String(),
			s.Blob.MimeType,
			s.Blob.Size,
		)
		if err != nil {
			return err
		}
	}

	if err := putReferences(tx, pull.AtUri(), pull.References); err != nil {
		return fmt.Errorf("put reference_links: %w", err)
	}
	return nil
}

func NextPullId(e Execer, repoDid string) (int, error) {
	var pullId int
	err := e.QueryRow(`select next_pull_id from repo_pull_seqs where repo_did = ?`, repoDid).Scan(&pullId)
	return pullId - 1, err
}

func GetPullsPaginated(e Execer, page pagination.Page, filters ...orm.Filter) ([]*models.Pull, error) {
	pulls := make(map[syntax.ATURI]*models.Pull)

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
		pageClause = fmt.Sprintf(
			" limit %d offset %d ",
			page.Limit,
			page.Offset,
		)
	}

	query := fmt.Sprintf(`
		select
			id,
			owner_did,
			repo_did,
			pull_id,
			created,
			title,
			state,
			target_branch,
			body,
			rkey,
			source_branch,
			source_repo_did,
			dependent_on
		from
			pulls
		%s
		order by
			created desc
		%s
	`, whereClause, pageClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var pull models.Pull
		var createdAt string
		var sourceBranch, sourceRepoDid, dependentOn sql.NullString
		err := rows.Scan(
			&pull.ID,
			&pull.OwnerDid,
			&pull.RepoDid,
			&pull.PullId,
			&createdAt,
			&pull.Title,
			&pull.State,
			&pull.TargetBranch,
			&pull.Body,
			&pull.Rkey,
			&sourceBranch,
			&sourceRepoDid,
			&dependentOn,
		)
		if err != nil {
			return nil, err
		}

		createdTime, err := time.Parse(time.RFC3339, createdAt)
		if err != nil {
			return nil, err
		}
		pull.Created = createdTime

		if sourceBranch.Valid {
			pull.PullSource = &models.PullSource{
				Branch: sourceBranch.String,
			}
			if sourceRepoDid.Valid {
				sourceRepoDidParsed, err := syntax.ParseDID(sourceRepoDid.String)
				if err != nil {
					return nil, err
				}
				pull.PullSource.RepoDid = &sourceRepoDidParsed
			}
		}

		if dependentOn.Valid {
			x := syntax.ATURI(dependentOn.String)
			pull.DependentOn = &x
		}

		pulls[pull.AtUri()] = &pull
	}

	var pullAts []syntax.ATURI
	for _, p := range pulls {
		pullAts = append(pullAts, p.AtUri())
	}
	submissionsMap, err := GetPullSubmissions(e, orm.FilterIn("pull_at", pullAts))
	if err != nil {
		return nil, fmt.Errorf("failed to get submissions: %w", err)
	}

	for pullAt, submissions := range submissionsMap {
		if p, ok := pulls[pullAt]; ok {
			p.Submissions = submissions
		}
	}

	// collect allLabels for each issue
	allLabels, err := GetLabels(e, orm.FilterIn("subject", pullAts))
	if err != nil {
		return nil, fmt.Errorf("failed to query labels: %w", err)
	}
	for pullAt, labels := range allLabels {
		if p, ok := pulls[pullAt]; ok {
			p.Labels = labels
		}
	}

	// build up reverse mappings: p.Repo and p.PullSource.Repo
	var repoDids []syntax.DID
	for _, p := range pulls {
		repoDids = append(repoDids, p.RepoDid)
		if p.PullSource != nil && p.PullSource.RepoDid != nil {
			repoDids = append(repoDids, *p.PullSource.RepoDid)
		}
	}

	repos, err := GetRepos(e, orm.FilterIn("repo_did", repoDids))
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return nil, fmt.Errorf("failed to get repos: %w", err)
	}

	repoMap := make(map[syntax.DID]*models.Repo)
	for _, r := range repos {
		repoMap[syntax.DID(r.RepoDid)] = &r
	}

	for _, p := range pulls {
		if repo, ok := repoMap[p.RepoDid]; ok {
			p.Repo = repo
		}
		if p.PullSource != nil && p.PullSource.RepoDid != nil {
			if sourceRepo, ok := repoMap[*p.PullSource.RepoDid]; ok {
				p.PullSource.Repo = sourceRepo
			}
		}
	}

	allReferences, err := GetReferencesAll(e, orm.FilterIn("from_at", pullAts))
	if err != nil {
		return nil, fmt.Errorf("failed to query reference_links: %w", err)
	}
	for pullAt, references := range allReferences {
		if pull, ok := pulls[pullAt]; ok {
			pull.References = references
		}
	}

	orderedByPullId := []*models.Pull{}
	for _, p := range pulls {
		orderedByPullId = append(orderedByPullId, p)
	}
	sort.Slice(orderedByPullId, func(i, j int) bool {
		return orderedByPullId[i].PullId > orderedByPullId[j].PullId
	})

	return orderedByPullId, nil
}

func GetPulls(e Execer, filters ...orm.Filter) ([]*models.Pull, error) {
	return GetPullsPaginated(e, pagination.Page{}, filters...)
}

func GetPull(e Execer, filters ...orm.Filter) (*models.Pull, error) {
	pulls, err := GetPullsPaginated(e, pagination.Page{Limit: 1}, filters...)
	if err != nil {
		return nil, err
	}
	if len(pulls) == 0 {
		return nil, sql.ErrNoRows
	}

	return pulls[0], nil
}

// mapping from pull -> pull submissions
func GetPullSubmissions(e Execer, filters ...orm.Filter) (map[syntax.ATURI][]*models.PullSubmission, error) {
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

	query := fmt.Sprintf(`
		select
			id,
			pull_at,
			round_number,
			patch,
			combined,
			created,
			source_rev,
			patch_blob_ref,
			patch_blob_mime,
			patch_blob_size
		from
			pull_submissions
		%s
		order by
			round_number asc
		`, whereClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	submissionMap := make(map[int]*models.PullSubmission)

	for rows.Next() {
		var submission models.PullSubmission
		var submissionCreatedStr string
		var submissionSourceRev, submissionCombined sql.Null[string]
		var patchBlobRef, patchBlobMime sql.Null[string]
		var patchBlobSize sql.Null[int64]
		err := rows.Scan(
			&submission.ID,
			&submission.PullAt,
			&submission.RoundNumber,
			&submission.Patch,
			&submissionCombined,
			&submissionCreatedStr,
			&submissionSourceRev,
			&patchBlobRef,
			&patchBlobMime,
			&patchBlobSize,
		)
		if err != nil {
			return nil, err
		}

		if t, err := time.Parse(time.RFC3339, submissionCreatedStr); err == nil {
			submission.Created = t
		}

		if submissionSourceRev.Valid {
			submission.SourceRev = submissionSourceRev.V
		}

		if submissionCombined.Valid {
			submission.Combined = submissionCombined.V
		}

		if patchBlobRef.Valid {
			submission.Blob.Ref = lexutil.LexLink(cid.MustParse(patchBlobRef.V))
		}

		if patchBlobMime.Valid {
			submission.Blob.MimeType = patchBlobMime.V
		}

		if patchBlobSize.Valid {
			submission.Blob.Size = patchBlobSize.V
		}

		submissionMap[submission.ID] = &submission
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	// Get comments for all submissions using GetPullComments
	submissionIds := slices.Collect(maps.Keys(submissionMap))
	comments, err := GetPullComments(e, orm.FilterIn("submission_id", submissionIds))
	if err != nil {
		return nil, fmt.Errorf("failed to get pull comments: %w", err)
	}
	for _, comment := range comments {
		if submission, ok := submissionMap[comment.SubmissionId]; ok {
			submission.Comments = append(submission.Comments, comment)
		}
	}

	// group the submissions by pull_at
	m := make(map[syntax.ATURI][]*models.PullSubmission)
	for _, s := range submissionMap {
		m[s.PullAt] = append(m[s.PullAt], s)
	}

	// sort each one by round number
	for _, s := range m {
		slices.SortFunc(s, func(a, b *models.PullSubmission) int {
			return cmp.Compare(a.RoundNumber, b.RoundNumber)
		})
	}

	return m, nil
}

func GetPullComments(e Execer, filters ...orm.Filter) ([]models.PullComment, error) {
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

	query := fmt.Sprintf(`
		select
			id,
			pull_id,
			submission_id,
			repo_did,
			owner_did,
			comment_at,
			body,
			created
		from
			pull_comments
		%s
		order by
			created asc
		`, whereClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	commentMap := make(map[string]*models.PullComment)
	for rows.Next() {
		var comment models.PullComment
		var createdAt string
		err := rows.Scan(
			&comment.ID,
			&comment.PullId,
			&comment.SubmissionId,
			&comment.RepoDid,
			&comment.OwnerDid,
			&comment.CommentAt,
			&comment.Body,
			&createdAt,
		)
		if err != nil {
			return nil, err
		}

		if t, err := time.Parse(time.RFC3339, createdAt); err == nil {
			comment.Created = t
		}

		atUri := comment.AtUri().String()
		commentMap[atUri] = &comment
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	// collect references for each comments
	commentAts := slices.Collect(maps.Keys(commentMap))
	allReferences, err := GetReferencesAll(e, orm.FilterIn("from_at", commentAts))
	if err != nil {
		return nil, fmt.Errorf("failed to query reference_links: %w", err)
	}
	for commentAt, references := range allReferences {
		if comment, ok := commentMap[commentAt.String()]; ok {
			comment.References = references
		}
	}

	var comments []models.PullComment
	for _, c := range commentMap {
		comments = append(comments, *c)
	}

	sort.Slice(comments, func(i, j int) bool {
		return comments[i].Created.Before(comments[j].Created)
	})

	return comments, nil
}

// timeframe here is directly passed into the sql query filter, and any
// timeframe in the past should be negative; e.g.: "-3 months"
func GetPullsByOwnerDid(e Execer, did, timeframe string) ([]models.Pull, error) {
	var pulls []models.Pull

	rows, err := e.Query(`
			select
				p.owner_did,
				p.repo_did,
				p.pull_id,
				p.created,
				p.title,
				p.state,
				r.did,
				r.name,
				r.knot,
				r.rkey,
				r.created
			from
				pulls p
			join
				repos r on p.repo_did = r.repo_did
			where
				p.owner_did = ? and p.created >= date ('now', ?)
			order by
				p.created desc`, did, timeframe)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var pull models.Pull
		var repo models.Repo
		var pullCreatedAt, repoCreatedAt string
		err := rows.Scan(
			&pull.OwnerDid,
			&pull.RepoDid,
			&pull.PullId,
			&pullCreatedAt,
			&pull.Title,
			&pull.State,
			&repo.Did,
			&repo.Name,
			&repo.Knot,
			&repo.Rkey,
			&repoCreatedAt,
		)
		if err != nil {
			return nil, err
		}

		pullCreatedTime, err := time.Parse(time.RFC3339, pullCreatedAt)
		if err != nil {
			return nil, err
		}
		pull.Created = pullCreatedTime

		repoCreatedTime, err := time.Parse(time.RFC3339, repoCreatedAt)
		if err != nil {
			return nil, err
		}
		repo.Created = repoCreatedTime

		pull.Repo = &repo

		pulls = append(pulls, pull)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return pulls, nil
}

func NewPullComment(tx *sql.Tx, comment *models.PullComment) (int64, error) {
	query := `insert into pull_comments (owner_did, repo_did, submission_id, comment_at, pull_id, body) values (?, ?, ?, ?, ?, ?)`
	res, err := tx.Exec(
		query,
		comment.OwnerDid,
		comment.RepoDid,
		comment.SubmissionId,
		comment.CommentAt,
		comment.PullId,
		comment.Body,
	)
	if err != nil {
		return 0, err
	}

	i, err := res.LastInsertId()
	if err != nil {
		return 0, err
	}

	if err := putReferences(tx, comment.AtUri(), comment.References); err != nil {
		return 0, fmt.Errorf("put reference_links: %w", err)
	}

	return i, nil
}

// use with transaction
func SetPullsState(e Execer, pullState models.PullState, filters ...orm.Filter) error {
	var conditions []string
	var args []any

	args = append(args, pullState)
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}
	args = append(args, models.PullAbandoned) // only update state of non-deleted pulls
	args = append(args, models.PullMerged)    // only update state of non-merged pulls

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	query := fmt.Sprintf("update pulls set state = ? %s and state <> ? and state <> ?", whereClause)

	_, err := e.Exec(query, args...)
	return err
}

func ClosePulls(e Execer, filters ...orm.Filter) error {
	return SetPullsState(e, models.PullClosed, filters...)
}

func ReopenPulls(e Execer, filters ...orm.Filter) error {
	return SetPullsState(e, models.PullOpen, filters...)
}

func MergePulls(e Execer, filters ...orm.Filter) error {
	return SetPullsState(e, models.PullMerged, filters...)
}

func AbandonPulls(e Execer, filters ...orm.Filter) error {
	return SetPullsState(e, models.PullAbandoned, filters...)
}

func ResubmitPull(
	e Execer,
	pullAt syntax.ATURI,
	newRoundNumber int,
	newPatch string,
	combinedPatch string,
	newSourceRev string,
	blob *lexutil.LexBlob,
) error {
	_, err := e.Exec(`
		insert into pull_submissions (
			pull_at,
			round_number,
			patch,
			combined,
			source_rev,
			patch_blob_ref,
			patch_blob_mime,
			patch_blob_size
		)
		values (?, ?, ?, ?, ?, ?, ?, ?)
	`, pullAt, newRoundNumber, newPatch, combinedPatch, newSourceRev, blob.Ref.String(), blob.MimeType, blob.Size)

	return err
}

func SetDependentOn(e Execer, dependentOn syntax.ATURI, filters ...orm.Filter) error {
	var conditions []string
	var args []any

	args = append(args, dependentOn)

	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	query := fmt.Sprintf("update pulls set dependent_on = ? %s", whereClause)
	_, err := e.Exec(query, args...)

	return err
}

func GetPullCount(e Execer, repoDid string) (models.PullCount, error) {
	row := e.QueryRow(`
		select
			count(case when state = ? then 1 end) as open_count,
			count(case when state = ? then 1 end) as merged_count,
			count(case when state = ? then 1 end) as closed_count,
			count(case when state = ? then 1 end) as deleted_count
		from pulls
		where repo_did = ?`,
		models.PullOpen,
		models.PullMerged,
		models.PullClosed,
		models.PullAbandoned,
		repoDid,
	)

	var count models.PullCount
	if err := row.Scan(&count.Open, &count.Merged, &count.Closed, &count.Deleted); err != nil {
		return models.PullCount{Open: 0, Merged: 0, Closed: 0, Deleted: 0}, err
	}

	return count, nil
}

//	change-id     dependent_on
//
// 4       w      ,-------- at_uri(z)  (TOP)
// 3       z <----',------- at_uri(y)
// 2       y <-----',------ at_uri(x)
// 1       x <------'      nil         (BOT)
//
// `w` has no dependents, so it is the top of the stack
//
// this unfortunately does a db query for *each* pull of the stack,
// ideally this would be a recursive query, but in the interest of implementation simplicity,
// we took the less performant route
//
// TODO: make this less bad
func GetStack(e Execer, atUri syntax.ATURI) (models.Stack, error) {
	// first get the pull for the given at-uri
	pull, err := GetPull(e, orm.FilterEq("at_uri", atUri))
	if err != nil {
		return nil, err
	}

	// Collect all pulls in the stack by traversing up and down
	allPulls := []*models.Pull{pull}
	visited := sets.New[syntax.ATURI]()

	// Traverse up to find all dependents
	current := pull
	for {
		dependent, err := GetPull(e,
			orm.FilterEq("dependent_on", current.AtUri()),
			orm.FilterNotEq("state", models.PullAbandoned),
		)
		if err != nil || dependent == nil {
			break
		}
		if visited.Contains(dependent.AtUri()) {
			return allPulls, fmt.Errorf("circular dependency detected in stack")
		}
		allPulls = append(allPulls, dependent)
		visited.Insert(dependent.AtUri())
		current = dependent
	}

	// Traverse down to find all dependencies
	current = pull
	for current.DependentOn != nil {
		dependency, err := GetPull(
			e,
			orm.FilterEq("at_uri", current.DependentOn),
			orm.FilterNotEq("state", models.PullAbandoned),
		)

		if err != nil {
			return allPulls, fmt.Errorf("failed to find parent pull request, stack is malformed, missing PR: %s", current.DependentOn)
		}
		if visited.Contains(dependency.AtUri()) {
			return allPulls, fmt.Errorf("circular dependency detected in stack")
		}
		allPulls = append(allPulls, dependency)
		visited.Insert(dependency.AtUri())
		current = dependency
	}

	// sort the list: find the top and build ordered list
	atUriMap := make(map[syntax.ATURI]*models.Pull, len(allPulls))
	dependentMap := make(map[syntax.ATURI]*models.Pull, len(allPulls))

	for _, p := range allPulls {
		atUriMap[p.AtUri()] = p
		if p.DependentOn != nil {
			dependentMap[*p.DependentOn] = p
		}
	}

	// the top of the stack is the pull that no other pull depends on
	var topPull *models.Pull
	for _, maybeTop := range allPulls {
		if _, ok := dependentMap[maybeTop.AtUri()]; !ok {
			topPull = maybeTop
			break
		}
	}

	pulls := []*models.Pull{}
	for {
		pulls = append(pulls, topPull)
		if topPull.DependentOn != nil {
			if next, ok := atUriMap[*topPull.DependentOn]; ok {
				topPull = next
			} else {
				return pulls, fmt.Errorf("failed to find parent pull request, stack is malformed")
			}
		} else {
			break
		}
	}

	return pulls, nil
}

func GetAbandonedPulls(e Execer, atUri syntax.ATURI) ([]*models.Pull, error) {
	stack, err := GetStack(e, atUri)
	if err != nil {
		return nil, err
	}

	var abandoned []*models.Pull
	for _, p := range stack {
		if p.State == models.PullAbandoned {
			abandoned = append(abandoned, p)
		}
	}

	return abandoned, nil
}
