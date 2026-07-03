package db

import "database/sql"

// UpsertIssueSubscription inserts or updates a user's subscription for an issue.
// subscribed=true means they want notifications; subscribed=false means explicitly unsubscribed.
func UpsertIssueSubscription(e Execer, userDid string, issueId int64, subscribed bool) error {
	sub := 0
	if subscribed {
		sub = 1
	}
	_, err := e.Exec(`
		INSERT INTO issue_subscriptions (user_did, issue_id, subscribed)
		VALUES (?, ?, ?)
		ON CONFLICT(user_did, issue_id) DO UPDATE SET subscribed = excluded.subscribed
	`, userDid, issueId, sub)
	return err
}

// UpsertPullSubscription inserts or updates a user's subscription for a pull.
func UpsertPullSubscription(e Execer, userDid string, pullId int64, subscribed bool) error {
	sub := 0
	if subscribed {
		sub = 1
	}
	_, err := e.Exec(`
		INSERT INTO pull_subscriptions (user_did, pull_id, subscribed)
		VALUES (?, ?, ?)
		ON CONFLICT(user_did, pull_id) DO UPDATE SET subscribed = excluded.subscribed
	`, userDid, pullId, sub)
	return err
}

// GetIssueSubscription returns (subscribed, found, err).
// If no row exists, found=false (meaning no explicit subscription).
func GetIssueSubscription(e Execer, userDid string, issueId int64) (subscribed bool, found bool, err error) {
	var sub int
	err = e.QueryRow(`
		SELECT subscribed FROM issue_subscriptions
		WHERE user_did = ? AND issue_id = ?
	`, userDid, issueId).Scan(&sub)
	if err == sql.ErrNoRows {
		return false, false, nil
	}
	if err != nil {
		return false, false, err
	}
	return sub == 1, true, nil
}

// GetPullSubscription returns (subscribed, found, err).
func GetPullSubscription(e Execer, userDid string, pullId int64) (subscribed bool, found bool, err error) {
	var sub int
	err = e.QueryRow(`
		SELECT subscribed FROM pull_subscriptions
		WHERE user_did = ? AND pull_id = ?
	`, userDid, pullId).Scan(&sub)
	if err == sql.ErrNoRows {
		return false, false, nil
	}
	if err != nil {
		return false, false, err
	}
	return sub == 1, true, nil
}

// GetIssueSubscribers returns DIDs with subscribed=1 for an issue.
func GetIssueSubscribers(e Execer, issueId int64) ([]string, error) {
	rows, err := e.Query(`
		SELECT user_did FROM issue_subscriptions
		WHERE issue_id = ? AND subscribed = 1
	`, issueId)
	if err != nil {
		return nil, err
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

// GetIssueUnsubscribers returns DIDs with subscribed=0 for an issue (opted out).
func GetIssueUnsubscribers(e Execer, issueId int64) ([]string, error) {
	rows, err := e.Query(`
		SELECT user_did FROM issue_subscriptions
		WHERE issue_id = ? AND subscribed = 0
	`, issueId)
	if err != nil {
		return nil, err
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

// GetPullSubscribers returns DIDs with subscribed=1 for a pull.
func GetPullSubscribers(e Execer, pullId int64) ([]string, error) {
	rows, err := e.Query(`
		SELECT user_did FROM pull_subscriptions
		WHERE pull_id = ? AND subscribed = 1
	`, pullId)
	if err != nil {
		return nil, err
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

// GetPullUnsubscribers returns DIDs with subscribed=0 for a pull (opted out).
func GetPullUnsubscribers(e Execer, pullId int64) ([]string, error) {
	rows, err := e.Query(`
		SELECT user_did FROM pull_subscriptions
		WHERE pull_id = ? AND subscribed = 0
	`, pullId)
	if err != nil {
		return nil, err
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
