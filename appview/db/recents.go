package db

import (
	"context"
	"fmt"
	"time"

	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func UpsertRecentLink(e Execer, userDid string, linkType models.RecentLinkType, target string) error {
	_, err := e.Exec(`
		insert into recent_links (user_did, link_type, target, visited)
		values (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		on conflict(user_did, target) do update set
			visited = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
	`, userDid, string(linkType), target)
	if err != nil {
		return fmt.Errorf("failed to upsert recent link: %w", err)
	}

	_, err = e.Exec(`
		delete from recent_links
		where user_did = ?
		  and id not in (
		    select id from recent_links
		    where user_did = ?
		    order by visited desc
		    limit 5
		  )
	`, userDid, userDid)
	if err != nil {
		return fmt.Errorf("failed to trim recent links: %w", err)
	}

	return nil
}

func GetRecentLinks(e Execer, filters ...orm.Filter) ([]*models.RecentLink, error) {
	var conditions []string
	var args []any

	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if len(conditions) > 0 {
		whereClause = "WHERE " + conditions[0]
		for _, condition := range conditions[1:] {
			whereClause += " AND " + condition
		}
	}

	args = append(args, 5)

	query := fmt.Sprintf(`
		select id, user_did, link_type, target, visited
		from recent_links
		%s
		order by visited desc
		limit ?
	`, whereClause)

	rows, err := e.QueryContext(context.Background(), query, args...)
	if err != nil {
		return nil, fmt.Errorf("failed to query recent links: %w", err)
	}
	defer rows.Close()

	var links []*models.RecentLink
	for rows.Next() {
		var l models.RecentLink
		var linkTypeStr string
		var visitedStr string
		if err := rows.Scan(&l.Id, &l.UserDid, &linkTypeStr, &l.Target, &visitedStr); err != nil {
			return nil, fmt.Errorf("failed to scan recent link: %w", err)
		}
		l.LinkType = models.RecentLinkType(linkTypeStr)
		l.Visited, err = time.Parse(time.RFC3339, visitedStr)
		if err != nil {
			return nil, fmt.Errorf("failed to parse visited timestamp: %w", err)
		}
		links = append(links, &l)
	}

	return links, nil
}
