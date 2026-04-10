package db

import (
	"database/sql"
	"fmt"
	"log"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/ipfs/go-cid"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/orm"
)

func AddVouch(e Execer, vouch *models.Vouch) error {
	query := `insert or replace into vouches (did, subject_did, cid, kind, reason) values (?, ?, ?, ?, ?)`
	_, err := e.Exec(query, vouch.Did, vouch.SubjectDid, vouch.Cid.String(), vouch.Kind, vouch.Reason)
	return err
}

func GetVouch(e Execer, did, subjectDid string) (*models.Vouch, error) {
	vouches, err := GetVouches(e, pagination.Page{Limit: 1},
		orm.FilterEq("did", did),
		orm.FilterEq("subject_did", subjectDid),
	)
	if err != nil {
		return nil, err
	}
	if len(vouches) == 0 {
		return nil, sql.ErrNoRows
	}
	return &vouches[0], nil
}

func GetVouches(e Execer, page pagination.Page, filters ...orm.Filter) ([]models.Vouch, error) {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if len(conditions) > 0 {
		whereClause = "where " + strings.Join(conditions, " and ")
	}

	pageClause := ""
	if page.Limit > 0 {
		pageClause = fmt.Sprintf("limit %d offset %d", page.Limit, page.Offset)
	}

	query := fmt.Sprintf(
		`select did, subject_did, cid, kind, reason, created_at
		from vouches
		%s
		order by created_at desc
		%s`,
		whereClause, pageClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var vouches []models.Vouch
	for rows.Next() {
		var v models.Vouch
		var cidStr string
		var createdAt string
		var reason sql.NullString

		if err := rows.Scan(&v.Did, &v.SubjectDid, &cidStr, &v.Kind, &reason, &createdAt); err != nil {
			log.Println("error scanning vouch:", err)
			continue
		}

		v.Cid, err = cid.Parse(cidStr)
		if err != nil {
			log.Println("unable to parse CID:", err)
			continue
		}

		t, err := time.Parse(time.RFC3339, createdAt)
		if err != nil {
			log.Println("unable to determine created at time")
			v.CreatedAt = time.Now()
		} else {
			v.CreatedAt = t
		}

		if reason.Valid {
			v.Reason = &reason.String
		}

		vouches = append(vouches, v)
	}
	return vouches, nil
}

func DeleteVouch(e Execer, did, subjectDid string) error {
	_, err := e.Exec(`delete from vouches where did = ? and subject_did = ?`, did, subjectDid)
	return err
}

func DeleteVouchByRkey(e Execer, did, rkey string) error {
	_, err := e.Exec(`delete from vouches where did = ? and subject_did = ?`, did, rkey)
	return err
}

func GetNetworkVouchTimeline(e Execer, viewerDid, profileDid string, page pagination.Page) ([]models.Vouch, error) {
	pageClause := ""
	if page.Limit > 0 {
		pageClause = fmt.Sprintf("limit %d offset %d", page.Limit, page.Offset)
	}

	query := fmt.Sprintf(
		`select did, subject_did, cid, kind, reason, created_at
		from vouches
		where (
			subject_did = ? and did in (select subject_did from vouches where did = ? and kind = 'vouch')
		) or (
			did = ? and subject_did in (select subject_did from vouches where did = ? and kind = 'vouch')
		)
		order by created_at desc
		%s`,
		pageClause)

	rows, err := e.Query(query, profileDid, viewerDid, profileDid, viewerDid)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var vouches []models.Vouch
	for rows.Next() {
		var v models.Vouch
		var cidStr string
		var createdAt string
		var reason sql.NullString

		if err := rows.Scan(&v.Did, &v.SubjectDid, &cidStr, &v.Kind, &reason, &createdAt); err != nil {
			log.Println("error scanning vouch:", err)
			continue
		}

		v.Cid, err = cid.Parse(cidStr)
		if err != nil {
			log.Println("unable to parse CID:", err)
			continue
		}

		t, err := time.Parse(time.RFC3339, createdAt)
		if err != nil {
			log.Println("unable to determine created at time")
			v.CreatedAt = time.Now()
		} else {
			v.CreatedAt = t
		}

		if reason.Valid {
			v.Reason = &reason.String
		}

		vouches = append(vouches, v)
	}
	return vouches, nil
}

func GetVouchRelationshipsBatch(e Execer, viewerDid syntax.DID, subjectDids []syntax.DID) (map[syntax.DID]*models.VouchRelationship, error) {
	if viewerDid == "" {
		return nil, fmt.Errorf("viewerDid cannot be empty")
	}

	result := make(map[syntax.DID]*models.VouchRelationship)
	for _, subjectDid := range subjectDids {
		result[subjectDid] = &models.VouchRelationship{
			ViewerDid:      viewerDid,
			SubjectDid:     subjectDid,
			NetworkVouches: []models.Vouch{},
		}
	}

	if len(subjectDids) == 0 {
		return result, nil
	}

	directVouches, err := GetVouches(e, pagination.Page{},
		orm.FilterEq("did", viewerDid),
		orm.FilterIn("subject_did", subjectDids),
	)
	if err != nil {
		return nil, err
	}
	for _, v := range directVouches {
		if rel, ok := result[v.SubjectDid]; ok {
			rel.NetworkVouches = append(rel.NetworkVouches, v)
		}
	}

	networkVouches, err := GetVouches(e, pagination.Page{},
		orm.FilterEq("did", viewerDid),
		orm.FilterEq("kind", string(models.VouchKindVouch)),
	)
	if err != nil {
		return nil, err
	}

	network := make([]syntax.DID, 0, len(networkVouches))
	for _, v := range networkVouches {
		network = append(network, v.SubjectDid)
	}

	if len(network) > 0 {
		networkToSubject, err := GetVouches(e, pagination.Page{},
			orm.FilterIn("subject_did", subjectDids),
			orm.FilterIn("did", network),
		)
		if err != nil {
			return nil, err
		}
		for _, v := range networkToSubject {
			if rel, ok := result[v.SubjectDid]; ok {
				rel.NetworkVouches = append(rel.NetworkVouches, v)
			}
		}
	}

	return result, nil
}

func GetVouchRelationship(e Execer, viewerDid, subjectDid syntax.DID) (*models.VouchRelationship, error) {
	batch, err := GetVouchRelationshipsBatch(e, viewerDid, []syntax.DID{subjectDid})
	if err != nil {
		return nil, err
	}
	return batch[subjectDid], nil
}

// priority:
//  1. collaborator invites sent
//  2. knot member invites sent
//  3. PR authors on FOO's repositories
//  4. issue authors on FOO's repositories
//  5. PR comment authors on FOO's repositories
//  6. issue comment authors on FOO's repositories
//  7. users FOO recently followed
//  8. owners of repositories FOO recently starred
func GetVouchSuggestions(e Execer, did string, limit int) ([]models.VouchSuggestion, error) {
	query := `
		select did, reason from (
			select subject_did as did, 1 as priority, created,
				'You invited this user to collaborate on a repository' as reason
			from collaborators
			where collaborators.did = ?
				and subject_did != ?

			union all

			select subject as did, 2 as priority, created,
				'You invited this user to your knot' as reason
			from spindle_members
			where spindle_members.did = ?
				and subject != ?

			union all

			select p.owner_did as did, 3 as priority, p.created,
				'This user opened a pull request on your repository' as reason
			from pulls p
			join repos r on r.at_uri = p.repo_at
			where r.did = ?
				and p.owner_did != ?

			union all

			select i.did as did, 4 as priority, i.created,
				'This user opened an issue on your repository' as reason
			from issues i
			join repos r on r.at_uri = i.repo_at
			where r.did = ?
				and i.did != ?

			union all

			select pc.owner_did as did, 5 as priority, pc.created,
				'This user commented on a pull request on your repository' as reason
			from pull_comments pc
			join repos r on r.at_uri = pc.repo_at
			where r.did = ?
				and pc.owner_did != ?

			union all

			select ic.did as did, 6 as priority, ic.created,
				'This user commented on an issue on your repository' as reason
			from issue_comments ic
			join issues i on i.at_uri = ic.issue_at
			join repos r on r.at_uri = i.repo_at
			where r.did = ?
				and ic.did != ?

			union all

			select f.subject_did as did, 7 as priority, f.followed_at as created,
				'You recently followed this user' as reason
			from follows f
			where f.user_did = ?
				and f.subject_did != ?

			union all

			select r.did as did, 8 as priority, s.created,
				'You recently starred a repository by this user' as reason
			from stars s
			join repos r on r.at_uri = s.subject_at
			where s.did = ?
				and r.did != ?
		)
		where did not in (
			select subject_did from vouches where vouches.did = ?
		)
		group by did
		order by min(priority) asc, max(created) desc
		limit ?
	`

	args := []any{
		did, did, // collaborators
		did, did, // spindle_members
		did, did, // pulls
		did, did, // issues
		did, did, // pull_comments
		did, did, // issue_comments
		did, did, // follows
		did, did, // stars
		did, // vouches exclusion
		limit,
	}

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, fmt.Errorf("GetVouchSuggestions: %w", err)
	}
	defer rows.Close()

	var suggestions []models.VouchSuggestion
	for rows.Next() {
		var s models.VouchSuggestion
		if err := rows.Scan(&s.Did, &s.Reason); err != nil {
			log.Println("error scanning vouch suggestion:", err)
			continue
		}
		suggestions = append(suggestions, s)
	}
	return suggestions, nil
}
