package db

import (
	"database/sql"
	"encoding/json"
	"slices"
	"strings"
	"time"
)

// the raw token is never stored, only its hash
type ExecutorToken struct {
	Name             string
	CreatedAt        string
	ExpiresAt        *time.Time
	Labels           []string
	QuarantineReason *string
	QuarantinedAt    *string
}

func (d *DB) AddExecutorToken(name, tokenHash string, expiresAt *time.Time, labels []string) error {
	normalized := normalizeLabels(labels)
	labelsBytes, err := json.Marshal(normalized)
	if err != nil {
		return err
	}
	_, err = d.Exec(
		`insert into mill_executors (name, token_hash, expires_at, labels) values (?, ?, ?, ?)`,
		name, tokenHash, expiryArg(expiresAt), string(labelsBytes),
	)
	return err
}

func (d *DB) ResolveExecutorToken(tokenHash string) (string, []string, bool, error) {
	var name string
	var expires sql.NullString
	var labelsRaw, quarantineReason sql.NullString
	err := d.QueryRow(
		`select name, expires_at, labels, quarantine_reason from mill_executors where token_hash = ?`, tokenHash,
	).Scan(&name, &expires, &labelsRaw, &quarantineReason)
	if err == sql.ErrNoRows {
		return "", nil, false, nil
	}
	if err != nil {
		return "", nil, false, err
	}
	if quarantineReason.Valid {
		return "", nil, false, nil
	}
	exp, hasExpiry, err := parseExpiry(expires)
	if err != nil {
		return "", nil, false, err
	}
	if hasExpiry && time.Now().After(exp) {
		return "", nil, false, nil
	}
	var labels []string
	if labelsRaw.Valid && labelsRaw.String != "" {
		if err := json.Unmarshal([]byte(labelsRaw.String), &labels); err != nil {
			return "", nil, false, err
		}
	}
	return name, labels, true, nil
}

func (d *DB) QuarantineExecutor(name, reason string) error {
	_, err := d.Exec(
		`update mill_executors
		 set quarantine_reason = ?, quarantined_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
		 where name = ?`,
		reason, name,
	)
	return err
}

func (d *DB) ClearExecutorQuarantine(name string) (bool, error) {
	res, err := d.Exec(
		`update mill_executors set quarantine_reason = null, quarantined_at = null where name = ?`,
		name,
	)
	if err != nil {
		return false, err
	}
	n, err := res.RowsAffected()
	return n > 0, err
}

func (d *DB) RevokeExecutorToken(name string) (bool, error) {
	res, err := d.Exec(`delete from mill_executors where name = ?`, name)
	if err != nil {
		return false, err
	}
	n, err := res.RowsAffected()
	return n > 0, err
}

func (d *DB) ListExecutorTokens() ([]ExecutorToken, error) {
	rows, err := d.Query(`select name, created_at, expires_at, labels, quarantine_reason, quarantined_at from mill_executors order by name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []ExecutorToken
	for rows.Next() {
		var t ExecutorToken
		var expires, labelsRaw, quarantineReason, quarantinedAt sql.NullString
		if err := rows.Scan(&t.Name, &t.CreatedAt, &expires, &labelsRaw, &quarantineReason, &quarantinedAt); err != nil {
			return nil, err
		}
		exp, hasExpiry, err := parseExpiry(expires)
		if err != nil {
			return nil, err
		}
		if hasExpiry {
			t.ExpiresAt = &exp
		}
		if labelsRaw.Valid && labelsRaw.String != "" {
			if err := json.Unmarshal([]byte(labelsRaw.String), &t.Labels); err != nil {
				return nil, err
			}
		}
		if quarantineReason.Valid {
			t.QuarantineReason = &quarantineReason.String
		}
		if quarantinedAt.Valid {
			t.QuarantinedAt = &quarantinedAt.String
		}
		out = append(out, t)
	}
	return out, rows.Err()
}

func normalizeLabels(labels []string) []string {
	var out []string
	seen := make(map[string]bool)
	for _, l := range labels {
		trimmed := strings.TrimSpace(l)
		if trimmed == "" {
			continue
		}
		if !seen[trimmed] {
			seen[trimmed] = true
			out = append(out, trimmed)
		}
	}
	slices.Sort(out)
	return out
}

func expiryArg(t *time.Time) any {
	if t == nil {
		return nil
	}
	return t.UTC().Format(time.RFC3339)
}

func parseExpiry(s sql.NullString) (time.Time, bool, error) {
	if !s.Valid {
		return time.Time{}, false, nil
	}
	t, err := time.Parse(time.RFC3339, s.String)
	if err != nil {
		return time.Time{}, false, err
	}
	return t, true, nil
}
