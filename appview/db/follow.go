package db

import (
	"fmt"
	"log"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func UpsertFollow(e Execer, follow models.Follow) error {
	_, err := e.Exec(
		`insert into follows (did, rkey, subject_did, created)
		values (?, ?, ?, ?)
		on conflict(did, rkey) do update set
			subject_did = excluded.subject_did,
			created     = excluded.created`,
		follow.UserDid,
		follow.Rkey,
		follow.SubjectDid,
		follow.FollowedAt.Format(time.RFC3339),
	)
	return err
}

// Remove a follow
func DeleteFollow(e Execer, did, subjectDid syntax.DID) ([]syntax.ATURI, error) {
	var deleted []syntax.ATURI
	rows, err := e.Query(
		`delete from follows
		where did = ? and subject_did = ?
		returning at_uri`,
		did,
		subjectDid,
	)
	if err != nil {
		return nil, fmt.Errorf("deleting follows: %w", err)
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

// Remove a follow
func DeleteFollowByRkey(e Execer, userDid, rkey string) error {
	_, err := e.Exec(`delete from follows where did = ? and rkey = ?`, userDid, rkey)
	return err
}

func GetFollowerFollowingCount(e Execer, did string) (models.FollowStats, error) {
	var followers, following int64
	err := e.QueryRow(
		`SELECT
		COUNT(CASE WHEN subject_did = ? THEN 1 END) AS followers,
		COUNT(CASE WHEN did = ? THEN 1 END) AS following
		FROM follows;`, did, did).Scan(&followers, &following)
	if err != nil {
		return models.FollowStats{}, err
	}
	return models.FollowStats{
		Followers: followers,
		Following: following,
	}, nil
}

func GetFollowerFollowingCounts(e Execer, dids []string) (map[string]models.FollowStats, error) {
	if len(dids) == 0 {
		return nil, nil
	}

	placeholders := make([]string, len(dids))
	for i := range placeholders {
		placeholders[i] = "?"
	}
	placeholderStr := strings.Join(placeholders, ",")

	args := make([]any, len(dids)*2)
	for i, did := range dids {
		args[i] = did
		args[i+len(dids)] = did
	}

	query := fmt.Sprintf(`
		select
			coalesce(f.did, g.did) as did,
			coalesce(f.followers, 0) as followers,
			coalesce(g.following, 0) as following
		from (
			select subject_did as did, count(*) as followers
			from follows
			where subject_did in (%s)
			group by subject_did
		) f
		full outer join (
			select did as did, count(*) as following
			from follows
			where did in (%s)
			group by did
		) g on f.did = g.did`,
		placeholderStr, placeholderStr)

	result := make(map[string]models.FollowStats)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var did string
		var followers, following int64
		if err := rows.Scan(&did, &followers, &following); err != nil {
			return nil, err
		}
		result[did] = models.FollowStats{
			Followers: followers,
			Following: following,
		}
	}

	for _, did := range dids {
		if _, exists := result[did]; !exists {
			result[did] = models.FollowStats{
				Followers: 0,
				Following: 0,
			}
		}
	}

	return result, nil
}

func GetFollows(e Execer, limit int, filters ...orm.Filter) ([]models.Follow, error) {
	var follows []models.Follow

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
	limitClause := ""
	if limit > 0 {
		limitClause = " limit ?"
		args = append(args, limit)
	}

	query := fmt.Sprintf(
		`select did, subject_did, created, rkey
		from follows
		%s
		order by created desc
		%s
	`, whereClause, limitClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var follow models.Follow
		var followedAt string
		err := rows.Scan(
			&follow.UserDid,
			&follow.SubjectDid,
			&followedAt,
			&follow.Rkey,
		)
		if err != nil {
			return nil, err
		}
		followedAtTime, err := time.Parse(time.RFC3339, followedAt)
		if err != nil {
			log.Println("unable to determine followed at time")
			follow.FollowedAt = time.Now()
		} else {
			follow.FollowedAt = followedAtTime
		}
		follows = append(follows, follow)
	}
	return follows, nil
}

func GetFollowers(e Execer, did string) ([]models.Follow, error) {
	return GetFollows(e, 0, orm.FilterEq("subject_did", did))
}

func GetFollowing(e Execer, did string) ([]models.Follow, error) {
	return GetFollows(e, 0, orm.FilterEq("did", did))
}

func getFollowStatuses(e Execer, userDid string, subjectDids []string) (map[string]models.FollowStatus, error) {
	if len(subjectDids) == 0 || userDid == "" {
		return make(map[string]models.FollowStatus), nil
	}

	result := make(map[string]models.FollowStatus)

	for _, subjectDid := range subjectDids {
		if userDid == subjectDid {
			result[subjectDid] = models.IsSelf
		} else {
			result[subjectDid] = models.IsNotFollowing
		}
	}

	var querySubjects []string
	for _, subjectDid := range subjectDids {
		if userDid != subjectDid {
			querySubjects = append(querySubjects, subjectDid)
		}
	}

	if len(querySubjects) == 0 {
		return result, nil
	}

	placeholders := make([]string, len(querySubjects))
	args := make([]any, len(querySubjects)+1)
	args[0] = userDid

	for i, subjectDid := range querySubjects {
		placeholders[i] = "?"
		args[i+1] = subjectDid
	}

	query := fmt.Sprintf(`
		SELECT subject_did
		FROM follows
		WHERE did = ? AND subject_did IN (%s)
	`, strings.Join(placeholders, ","))

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var subjectDid string
		if err := rows.Scan(&subjectDid); err != nil {
			return nil, err
		}
		result[subjectDid] = models.IsFollowing
	}

	return result, nil
}

func GetFollowStatus(e Execer, userDid, subjectDid string) models.FollowStatus {
	statuses, err := getFollowStatuses(e, userDid, []string{subjectDid})
	if err != nil {
		return models.IsNotFollowing
	}
	return statuses[subjectDid]
}

func GetFollowStatuses(e Execer, userDid string, subjectDids []string) (map[string]models.FollowStatus, error) {
	return getFollowStatuses(e, userDid, subjectDids)
}
