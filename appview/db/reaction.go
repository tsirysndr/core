package db

import (
	"fmt"
	"log"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func AddReaction(e Execer, reactedByDid string, threadAt syntax.ATURI, kind models.ReactionKind, rkey string, created time.Time) error {
	query := `insert or ignore into reactions (reacted_by_did, thread_at, kind, rkey, created) values (?, ?, ?, ?, ?)`
	_, err := e.Exec(query, reactedByDid, threadAt, kind, rkey, created.UTC().Format(time.RFC3339))
	return err
}

// Get a reaction record
func GetReaction(e Execer, reactedByDid string, threadAt syntax.ATURI, kind models.ReactionKind) (*models.Reaction, error) {
	query := `
	select reacted_by_did, thread_at, created, rkey
	from reactions
	where reacted_by_did = ? and thread_at = ? and kind = ?`
	row := e.QueryRow(query, reactedByDid, threadAt, kind)

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
func DeleteReaction(e Execer, reactedByDid string, threadAt syntax.ATURI, kind models.ReactionKind) error {
	_, err := e.Exec(`delete from reactions where reacted_by_did = ? and thread_at = ? and kind = ?`, reactedByDid, threadAt, kind)
	return err
}

// Remove a reaction
func DeleteReactionByRkey(e Execer, reactedByDid string, rkey string) error {
	_, err := e.Exec(`delete from reactions where reacted_by_did = ? and rkey = ?`, reactedByDid, rkey)
	return err
}

func GetReactionCount(e Execer, threadAt syntax.ATURI) (int, error) {
	count := 0
	err := e.QueryRow(`select count(reacted_by_did) from reactions where thread_at = ?`, threadAt).Scan(&count)
	if err != nil {
		return 0, err
	}
	return count, nil
}

func GetReactionCountByKind(e Execer, threadAt syntax.ATURI, kind models.ReactionKind) (int, error) {
	count := 0
	err := e.QueryRow(
		`select count(reacted_by_did) from reactions where thread_at = ? and kind = ?`, threadAt, kind).Scan(&count)
	if err != nil {
		return 0, err
	}
	return count, nil
}

// GetReactionDisplayDataMap returns map of [models.ReactionKind]->[models.ReactionDisplayData]
func GetReactionMap(e Execer, userLimit int, threadAt syntax.ATURI) (map[models.ReactionKind]models.ReactionDisplayData, error) {
	reactionMaps, err := ListReactionDisplayDataMap(e, []syntax.ATURI{threadAt}, userLimit)
	return reactionMaps[threadAt], err
}

// ListReactionDisplayDataMap returns map of [syntax.ATURI]->[models.ReactionKind]->[models.ReactionDisplayData]
func ListReactionDisplayDataMap(e Execer, threads []syntax.ATURI, userLimit int) (map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData, error) {
	if len(threads) == 0 {
		return nil, nil
	}

	filter := orm.FilterIn("thread_at", threads)
	args := filter.Arg()
	args = append(args, userLimit)
	rows, err := e.Query(
		fmt.Sprintf(
			`with ranked_reactions as (
				select
					thread_at,
					kind,
					reacted_by_did,
					row_number() over (partition by thread_at, kind order by created asc) as rn,
					count(*) over (partition by thread_at, kind) as total
				from reactions
				where %s
			)
			select thread_at, kind, reacted_by_did, total
			from ranked_reactions
			where rn <= ?
			order by thread_at, kind, rn asc`,
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
func GetReactionStatusMap(e Execer, userDid syntax.DID, threadAt syntax.ATURI) (map[models.ReactionKind]bool, error) {
	reactionMaps, err := ListReactionStatusMap(e, []syntax.ATURI{threadAt}, userDid)
	return reactionMaps[threadAt], err
}

// ListReactionStatusMap returns map of [syntax.ATURI]->[models.ReactionKind]->[bool]
func ListReactionStatusMap(e Execer, threads []syntax.ATURI, userDid syntax.DID) (map[syntax.ATURI]map[models.ReactionKind]bool, error) {
	if len(threads) == 0 {
		return nil, nil
	}

	filter := orm.FilterIn("thread_at", threads)
	args := []any{userDid}
	args = append(args, filter.Arg()...)
	rows, err := e.Query(
		fmt.Sprintf(
			`select thread_at, kind from reactions
			where reacted_by_did = ? and %s`,
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
