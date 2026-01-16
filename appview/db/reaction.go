package db

import (
	"fmt"
	"log"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func AddReaction(e Execer, did string, subjectAt syntax.ATURI, kind models.ReactionKind, rkey string, created time.Time) error {
	query := `insert or ignore into reactions (did, subject_at, kind, rkey, created) values (?, ?, ?, ?, ?)`
	_, err := e.Exec(query, did, subjectAt, kind, rkey, created.UTC().Format(time.RFC3339))
	return err
}

// Get a reaction record
func GetReaction(e Execer, did string, subjectAt syntax.ATURI, kind models.ReactionKind) (*models.Reaction, error) {
	query := `
	select did, subject_at, created, rkey
	from reactions
	where did = ? and subject_at = ? and kind = ?`
	row := e.QueryRow(query, did, subjectAt, kind)

	var reaction models.Reaction
	var created string
	err := row.Scan(&reaction.ReactedByDid, &reaction.ThreadAt, &created, &reaction.Rkey)
	if err != nil {
		return nil, err
	}

	createdAtTime, err := time.Parse(time.RFC3339, created)
	if err != nil {
		log.Println("unable to determine followed at time")
		reaction.Created = time.Now()
	} else {
		reaction.Created = createdAtTime
	}

	return &reaction, nil
}

// Remove a reaction
func DeleteReaction(e Execer, did string, subjectAt syntax.ATURI, kind models.ReactionKind) error {
	_, err := e.Exec(`delete from reactions where did = ? and subject_at = ? and kind = ?`, did, subjectAt, kind)
	return err
}

// Remove a reaction
func DeleteReactionByRkey(e Execer, did string, rkey string) error {
	_, err := e.Exec(`delete from reactions where did = ? and rkey = ?`, did, rkey)
	return err
}

func GetReactionCount(e Execer, subjectAt syntax.ATURI) (int, error) {
	count := 0
	err := e.QueryRow(`select count(did) from reactions where subject_at = ?`, subjectAt).Scan(&count)
	if err != nil {
		return 0, err
	}
	return count, nil
}

func GetReactionCountByKind(e Execer, subjectAt syntax.ATURI, kind models.ReactionKind) (int, error) {
	count := 0
	err := e.QueryRow(
		`select count(did) from reactions where subject_at = ? and kind = ?`, subjectAt, kind).Scan(&count)
	if err != nil {
		return 0, err
	}
	return count, nil
}

// GetReactionDisplayDataMap returns map of [models.ReactionKind]->[models.ReactionDisplayData]
func GetReactionMap(e Execer, userLimit int, subjectAt syntax.ATURI) (map[models.ReactionKind]models.ReactionDisplayData, error) {
	reactionMaps, err := ListReactionDisplayDataMap(e, []syntax.ATURI{subjectAt}, userLimit)
	return reactionMaps[subjectAt], err
}

// ListReactionDisplayDataMap returns map of [syntax.ATURI]->[models.ReactionKind]->[models.ReactionDisplayData]
func ListReactionDisplayDataMap(e Execer, threads []syntax.ATURI, userLimit int) (map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData, error) {
	if len(threads) == 0 {
		return nil, nil
	}

	filter := orm.FilterIn("subject_at", threads)
	args := filter.Arg()
	args = append(args, userLimit)
	rows, err := e.Query(
		fmt.Sprintf(
			`with ranked_reactions as (
				select
					subject_at,
					kind,
					did,
					row_number() over (partition by subject_at, kind order by created asc) as rn,
					count(*) over (partition by subject_at, kind) as total
				from reactions
				where %s
			)
			select subject_at, kind, did, total
			from ranked_reactions
			where rn <= ?
			order by subject_at, kind, rn asc`,
			filter.Condition(),
		),
		args...,
	)
	if err != nil {
		return nil, fmt.Errorf("querying: %w", err)
	}
	defer rows.Close()

	// aturi -> kind -> {count,users}
	result := make(map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData)

	for rows.Next() {
		var aturi syntax.ATURI
		var kind models.ReactionKind
		var did syntax.DID
		var count int

		if err := rows.Scan(&aturi, &kind, &did, &count); err != nil {
			return nil, fmt.Errorf("scanning row: %w", err)
		}

		if _, ok := result[aturi]; !ok {
			result[aturi] = make(map[models.ReactionKind]models.ReactionDisplayData)
		}
		data := result[aturi][kind]
		data.Count = count
		data.Users = append(data.Users, did.String())
		result[aturi][kind] = data
	}

	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("iterate rows: %w", err)
	}

	return result, nil
}

// GetReactionStatusMap returns map of [models.ReactionKind]->[bool]
func GetReactionStatusMap(e Execer, userDid syntax.DID, subjectAt syntax.ATURI) (map[models.ReactionKind]bool, error) {
	reactionMaps, err := ListReactionStatusMap(e, []syntax.ATURI{subjectAt}, userDid)
	return reactionMaps[subjectAt], err
}

// ListReactionStatusMap returns map of [syntax.ATURI]->[models.ReactionKind]->[bool]
func ListReactionStatusMap(e Execer, threads []syntax.ATURI, userDid syntax.DID) (map[syntax.ATURI]map[models.ReactionKind]bool, error) {
	if len(threads) == 0 {
		return nil, nil
	}

	filter := orm.FilterIn("subject_at", threads)
	args := []any{userDid}
	args = append(args, filter.Arg()...)
	rows, err := e.Query(
		fmt.Sprintf(
			`select subject_at, kind from reactions
			where did = ? and %s`,
			filter.Condition(),
		),
		args...,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	// aturi -> kind -> bool
	result := make(map[syntax.ATURI]map[models.ReactionKind]bool)

	for rows.Next() {
		var aturi syntax.ATURI
		var kind models.ReactionKind

		if err := rows.Scan(&aturi, &kind); err != nil {
			return nil, fmt.Errorf("scanning row: %w", err)
		}

		if _, ok := result[aturi]; !ok {
			result[aturi] = make(map[models.ReactionKind]bool)
		}

		result[aturi][kind] = true
	}

	return result, nil
}
