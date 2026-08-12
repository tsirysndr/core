package db

import (
	"database/sql"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/deliberi/models"
	"tangled.org/core/orm"
)

// CreateNotification inserts a row, deduped on (recipient_did, at_uri).
func CreateNotification(e Execer, n *models.Notification) error {
	query := `
		insert into notifications
			(recipient_did, at_uri, type, actor_did, repo_did, entity_at, entity_title, read)
		values (?, ?, ?, ?, ?, ?, ?, ?)
		on conflict(recipient_did, at_uri) do nothing
	`
	res, err := e.Exec(query,
		n.RecipientDid, n.AtUri, string(n.Type), n.ActorDid,
		n.RepoDid, n.EntityAt, n.EntityTitle, n.Read,
	)
	if err != nil {
		return fmt.Errorf("failed to create notification: %w", err)
	}
	if id, err := res.LastInsertId(); err == nil {
		n.ID = id
	}
	return nil
}

const notifCols = `id, recipient_did, at_uri, type, actor_did, repo_did, entity_at, entity_title, read, emailed, created`

func scanNotification(rows interface{ Scan(...any) error }) (*models.Notification, error) {
	var n models.Notification
	var typeStr, createdStr string
	if err := rows.Scan(
		&n.ID, &n.RecipientDid, &n.AtUri, &typeStr, &n.ActorDid,
		&n.RepoDid, &n.EntityAt, &n.EntityTitle, &n.Read, &n.Emailed, &createdStr,
	); err != nil {
		return nil, err
	}
	n.Type = models.NotificationType(typeStr)
	n.Created, _ = time.Parse(time.RFC3339, createdStr)
	return &n, nil
}

func GetNotifications(e Execer, recipientDid string, limit int, filters ...orm.Filter) ([]*models.Notification, error) {
	conds := []string{"recipient_did = ?"}
	args := []any{recipientDid}
	for _, f := range filters {
		conds = append(conds, f.Condition())
		args = append(args, f.Arg()...)
	}
	query := fmt.Sprintf("select %s from notifications where %s order by created desc", notifCols, strings.Join(conds, " and "))
	if limit > 0 {
		query += fmt.Sprintf(" limit %d", limit)
	}
	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*models.Notification
	for rows.Next() {
		n, err := scanNotification(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, n)
	}
	return out, rows.Err()
}

func CountNotifications(e Execer, recipientDid string, filters ...orm.Filter) (int64, error) {
	conds := []string{"recipient_did = ?"}
	args := []any{recipientDid}
	for _, f := range filters {
		conds = append(conds, f.Condition())
		args = append(args, f.Arg()...)
	}
	query := fmt.Sprintf("select count(*) from notifications where %s", strings.Join(conds, " and "))
	var count int64
	if err := e.QueryRow(query, args...).Scan(&count); err != nil {
		return 0, err
	}
	return count, nil
}

func MarkRead(e Execer, recipientDid, atUri string, read bool) error {
	_, err := e.Exec(`update notifications set read = ? where recipient_did = ? and at_uri = ?`, read, recipientDid, atUri)
	return err
}

func MarkAllRead(e Execer, recipientDid string) error {
	_, err := e.Exec(`update notifications set read = 1 where recipient_did = ? and read = 0`, recipientDid)
	return err
}

func MarkEmailed(e Execer, ids []int64) error {
	if len(ids) == 0 {
		return nil
	}
	ph := make([]string, len(ids))
	args := make([]any, len(ids))
	for i, id := range ids {
		ph[i] = "?"
		args[i] = id
	}
	_, err := e.Exec(fmt.Sprintf(`update notifications set emailed = 1 where id in (%s)`, strings.Join(ph, ", ")), args...)
	return err
}

func GetPendingEmailDigestRecipients(e Execer, olderThan time.Time) ([]string, error) {
	ph := make([]string, len(models.EmailNotificationTypes))
	args := []any{olderThan.UTC().Format(time.RFC3339)}
	for i, t := range models.EmailNotificationTypes {
		ph[i] = "?"
		args = append(args, string(t))
	}
	query := fmt.Sprintf(`
		select distinct n.recipient_did
		from notifications n
		join notification_preferences np on np.user_did = n.recipient_did
		join emails em on em.did = n.recipient_did and em.is_primary = 1 and em.verified = 1
		where n.emailed = 0 and n.read = 0 and n.created < ?
		  and np.email_notifications = 1
		  and n.type in (%s)
	`, strings.Join(ph, ", "))
	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to query digest recipients: %w", err)
	}
	defer rows.Close()
	var dids []string
	for rows.Next() {
		var did string
		if err := rows.Scan(&did); err != nil {
			return nil, err
		}
		dids = append(dids, did)
	}
	return dids, rows.Err()
}

func GetPendingNotificationsForEmailDigest(e Execer, recipientDid string, olderThan time.Time) ([]*models.Notification, error) {
	ph := make([]string, len(models.EmailNotificationTypes))
	args := []any{recipientDid, olderThan.UTC().Format(time.RFC3339)}
	for i, t := range models.EmailNotificationTypes {
		ph[i] = "?"
		args = append(args, string(t))
	}
	query := fmt.Sprintf(`
		select %s from notifications
		where recipient_did = ? and emailed = 0 and read = 0 and created < ?
		  and type in (%s)
		order by created desc
	`, notifCols, strings.Join(ph, ", "))
	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*models.Notification
	for rows.Next() {
		n, err := scanNotification(rows)
		if err != nil {
			return nil, err
		}
		// repo names are not stored on the row: resolve them here, where the
		// digest needs them, so a rename shows up in the next email.
		n.RepoName = GetRepoName(e, n.RepoDid)
		out = append(out, n)
	}
	return out, rows.Err()
}

func GetNotificationPreference(e Execer, userDid string) (*models.NotificationPreferences, error) {
	query := `
		select id, user_did, repo_starred, issue_created, issue_commented, pull_created,
			pull_commented, followed, pull_merged, issue_closed, user_mentioned, email_notifications
		from notification_preferences
		where user_did = ?
	`
	var p models.NotificationPreferences
	var userDidStr string
	err := e.QueryRow(query, userDid).Scan(
		&p.ID, &userDidStr, &p.RepoStarred, &p.IssueCreated, &p.IssueCommented, &p.PullCreated,
		&p.PullCommented, &p.Followed, &p.PullMerged, &p.IssueClosed, &p.UserMentioned, &p.EmailNotifications,
	)
	if errors.Is(err, sql.ErrNoRows) {
		// no row yet: defaults so reads never fail for an uncustomized user
		return models.DefaultNotificationPreferences(syntax.DID(userDid)), nil
	}
	if err != nil {
		// a real failure must not read as "user wants everything": callers
		// deliver on these prefs, so opting out has to survive a db error.
		return nil, fmt.Errorf("failed to query notification preferences: %w", err)
	}
	p.UserDid = syntax.DID(userDidStr)
	return &p, nil
}

func UpsertNotificationPreferences(e Execer, prefs *models.NotificationPreferences) error {
	query := `
		insert into notification_preferences
			(user_did, repo_starred, issue_created, issue_commented, pull_created,
			 pull_commented, followed, pull_merged, issue_closed, user_mentioned, email_notifications)
		values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		on conflict(user_did) do update set
			repo_starred = excluded.repo_starred,
			issue_created = excluded.issue_created,
			issue_commented = excluded.issue_commented,
			pull_created = excluded.pull_created,
			pull_commented = excluded.pull_commented,
			followed = excluded.followed,
			pull_merged = excluded.pull_merged,
			issue_closed = excluded.issue_closed,
			user_mentioned = excluded.user_mentioned,
			email_notifications = excluded.email_notifications
	`
	_, err := e.Exec(query,
		prefs.UserDid.String(),
		prefs.RepoStarred, prefs.IssueCreated, prefs.IssueCommented, prefs.PullCreated,
		prefs.PullCommented, prefs.Followed, prefs.PullMerged, prefs.IssueClosed,
		prefs.UserMentioned, prefs.EmailNotifications,
	)
	return err
}
