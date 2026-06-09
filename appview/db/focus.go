package db

import (
	"database/sql"
	"errors"
	"fmt"
	"strings"
	"time"

	"tangled.org/core/appview/models"
)

// notification types that qualify for focus mode
var FocusEligibleTypes = []models.NotificationType{
	models.NotificationTypeIssueCreated,
	models.NotificationTypeIssueReopen,
	models.NotificationTypeIssueCommented,
	models.NotificationTypePullCreated,
	models.NotificationTypePullReopen,
	models.NotificationTypePullCommented,
	models.NotificationTypeUserMentioned,
}

func focusEligiblePlaceholders() (string, []any) {
	placeholders := make([]string, len(FocusEligibleTypes))
	args := make([]any, len(FocusEligibleTypes))
	for i, t := range FocusEligibleTypes {
		placeholders[i] = "?"
		args[i] = string(t)
	}
	return strings.Join(placeholders, ", "), args
}

// marks a user as currently focusing
func BeginFocus(e Execer, did string) error {
	_, err := e.Exec(`insert or replace into focusing (did) values (?)`, did)
	if err != nil {
		return fmt.Errorf("BeginFocus: %w", err)
	}
	return nil
}

// remove the focusing flag for a user
func EndFocus(e Execer, did string) error {
	_, err := e.Exec(`delete from focusing where did = ?`, did)
	if err != nil {
		return fmt.Errorf("EndFocus: %w", err)
	}
	return nil
}

// whether a user is currently in focus mode
func GetFocusStatus(e Execer, did string) (bool, error) {
	var exists bool
	err := e.QueryRow(`select exists(select 1 from focusing where did = ?)`, did).Scan(&exists)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("GetFocusStatus: %w", err)
	}
	return exists, nil
}

// oldest unread focus-eligible notification for the user, with its related entity populated
//
// returns nil, nil when empty
func GetNextFocusItem(e Execer, did string) (*models.NotificationWithEntity, error) {
	placeholders, typeArgs := focusEligiblePlaceholders()

	query := fmt.Sprintf(`
		select
			n.id, n.recipient_did, n.actor_did, n.type, n.entity_type, n.entity_id,
			n.read, n.created, n.repo_id, n.issue_id, n.pull_id,
			r.id as r_id, r.did as r_did, r.rkey as r_rkey, r.name as r_name, r.description as r_description, r.website as r_website, r.topics as r_topics,
			i.id as i_id, i.did as i_did, i.issue_id as i_issue_id, i.title as i_title, i.open as i_open,
			p.id as p_id, p.owner_did as p_owner_did, p.pull_id as p_pull_id, p.title as p_title, p.state as p_state
		from notifications n
		left join repos r on n.repo_id = r.id
		left join issues i on n.issue_id = i.id
		left join pulls p on n.pull_id = p.id
		where n.recipient_did = ?
		  and n.read = 0
		  and n.type in (%s)
		order by n.created asc
		limit 1
	`, placeholders)

	args := append([]any{did}, typeArgs...)

	row := e.QueryRow(query, args...)

	var n models.Notification
	var typeStr string
	var createdStr string
	var repo models.Repo
	var issue models.Issue
	var pull models.Pull
	var rId, iId, pId sql.NullInt64
	var rDid, rRkey, rName, rDescription, rWebsite, rTopicStr sql.NullString
	var iDid sql.NullString
	var iIssueId sql.NullInt64
	var iTitle sql.NullString
	var iOpen sql.NullBool
	var pOwnerDid sql.NullString
	var pPullId sql.NullInt64
	var pTitle sql.NullString
	var pState sql.NullInt64

	err := row.Scan(
		&n.ID, &n.RecipientDid, &n.ActorDid, &typeStr, &n.EntityType, &n.EntityId,
		&n.Read, &createdStr, &n.RepoId, &n.IssueId, &n.PullId,
		&rId, &rDid, &rRkey, &rName, &rDescription, &rWebsite, &rTopicStr,
		&iId, &iDid, &iIssueId, &iTitle, &iOpen,
		&pId, &pOwnerDid, &pPullId, &pTitle, &pState,
	)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("GetNextFocusItem: %w", err)
	}

	n.Type = models.NotificationType(typeStr)
	n.Created, err = time.Parse(time.RFC3339, createdStr)
	if err != nil {
		return nil, fmt.Errorf("GetNextFocusItem: parse created: %w", err)
	}

	entry := &models.NotificationWithEntity{Notification: &n}

	if rId.Valid {
		repo.Id = rId.Int64
		if rDid.Valid {
			repo.Did = rDid.String
		}
		if rRkey.Valid {
			repo.Rkey = rRkey.String
		}
		if rName.Valid {
			repo.Name = rName.String
		}
		if rDescription.Valid {
			repo.Description = rDescription.String
		}
		if rWebsite.Valid {
			repo.Website = rWebsite.String
		}
		if rTopicStr.Valid {
			repo.Topics = strings.Fields(rTopicStr.String)
		}
		entry.Repo = &repo
	}

	if iId.Valid {
		issue.Id = iId.Int64
		if iDid.Valid {
			issue.Did = iDid.String
		}
		if iIssueId.Valid {
			issue.IssueId = int(iIssueId.Int64)
		}
		if iTitle.Valid {
			issue.Title = iTitle.String
		}
		if iOpen.Valid {
			issue.Open = iOpen.Bool
		}
		entry.Issue = &issue
	}

	if pId.Valid {
		pull.ID = int(pId.Int64)
		if pOwnerDid.Valid {
			pull.OwnerDid = pOwnerDid.String
		}
		if pPullId.Valid {
			pull.PullId = int(pPullId.Int64)
		}
		if pTitle.Valid {
			pull.Title = pTitle.String
		}
		if pState.Valid {
			pull.State = models.PullState(pState.Int64)
		}
		entry.Pull = &pull
	}

	return entry, nil
}

// returns the number of unread focus-eligible notifications for a user (not sure if we need this?)
func CountFocusNotifs(e Execer, did string) (int64, error) {
	placeholders, typeArgs := focusEligiblePlaceholders()

	query := fmt.Sprintf(`
		select count(1)
		from notifications
		where recipient_did = ?
		  and read = 0
		  and type in (%s)
	`, placeholders)

	args := append([]any{did}, typeArgs...)

	var count int64
	err := e.QueryRow(query, args...).Scan(&count)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, nil
	}
	if err != nil {
		return 0, fmt.Errorf("CountFocusNotifs: %w", err)
	}
	return count, nil
}
