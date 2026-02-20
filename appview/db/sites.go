package db

import (
	"database/sql"
	"errors"
	"fmt"
	"time"

	"tangled.org/core/appview/models"
)

var (
	ErrDomainTaken    = errors.New("domain is already claimed by another user")
	ErrDomainCooldown = errors.New("domain is in a 30-day cooldown period after being released")
	ErrAlreadyClaimed = errors.New("you already have an active domain claim; release it before claiming a new one")
)

func scanClaim(row *sql.Row) (*models.DomainClaim, error) {
	var claim models.DomainClaim
	var deletedStr sql.NullString

	if err := row.Scan(&claim.ID, &claim.Did, &claim.Domain, &deletedStr); err != nil {
		return nil, err
	}

	if deletedStr.Valid {
		t, err := time.Parse(time.RFC3339, deletedStr.String)
		if err != nil {
			return nil, fmt.Errorf("parsing deleted timestamp: %w", err)
		}
		claim.Deleted = &t
	}

	return &claim, nil
}

func GetDomainClaimByDomain(e Execer, domain string) (*models.DomainClaim, error) {
	row := e.QueryRow(`
		select id, did, domain, deleted
		from domain_claims
		where domain = ?
	`, domain)
	return scanClaim(row)
}

func GetActiveDomainClaimForDid(e Execer, did string) (*models.DomainClaim, error) {
	row := e.QueryRow(`
		select id, did, domain, deleted
		from domain_claims
		where did = ? and deleted is null
	`, did)
	return scanClaim(row)
}

func GetDomainClaimForDid(e Execer, did string) (*models.DomainClaim, error) {
	row := e.QueryRow(`
		select id, did, domain, deleted
		from domain_claims
		where did = ?
	`, did)
	return scanClaim(row)
}

func ClaimDomain(e Execer, did, domain string) error {
	const cooldown = 30 * 24 * time.Hour

	domainRow, err := GetDomainClaimByDomain(e, domain)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return fmt.Errorf("looking up domain: %w", err)
	}

	if domainRow != nil {
		if domainRow.Did == did {
			if domainRow.Deleted == nil {
				return nil
			}
			if time.Since(*domainRow.Deleted) < cooldown {
				return ErrDomainCooldown
			}
			_, err = e.Exec(`
				update domain_claims set deleted = null where did = ? and domain = ?
			`, did, domain)
			return err
		}

		if domainRow.Deleted == nil {
			return ErrDomainTaken
		}
		if time.Since(*domainRow.Deleted) < cooldown {
			return ErrDomainCooldown
		}

		if _, err = e.Exec(`delete from domain_claims where domain = ?`, domain); err != nil {
			return fmt.Errorf("clearing expired domain row: %w", err)
		}
	}

	didRow, err := GetDomainClaimForDid(e, did)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return fmt.Errorf("looking up DID claim: %w", err)
	}

	if didRow == nil {
		_, err = e.Exec(`
			insert into domain_claims (did, domain) values (?, ?)
		`, did, domain)
		return err
	}

	if didRow.Deleted == nil {
		return ErrAlreadyClaimed
	}

	_, err = e.Exec(`
		update domain_claims set domain = ?, deleted = null where did = ?
	`, domain, did)
	return err
}

func ReleaseDomain(e Execer, did, domain string) error {
	result, err := e.Exec(`
		update domain_claims
		set deleted = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
		where did = ? and domain = ? and deleted is null
	`, did, domain)
	if err != nil {
		return err
	}

	n, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if n == 0 {
		return errors.New("domain not found or not actively claimed by this account")
	}
	return nil
}

// GetRepoSiteConfig returns the site configuration for a repo, or nil if not configured.
func GetRepoSiteConfig(e Execer, repoAt string) (*models.RepoSite, error) {
	row := e.QueryRow(`
		select id, repo_at, branch, dir, is_index, created, updated
		from repo_sites
		where repo_at = ?
	`, repoAt)

	var s models.RepoSite
	var isIndex int
	var createdStr, updatedStr string

	err := row.Scan(&s.ID, &s.RepoAt, &s.Branch, &s.Dir, &isIndex, &createdStr, &updatedStr)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}

	s.IsIndex = isIndex != 0

	s.Created, err = time.Parse(time.RFC3339, createdStr)
	if err != nil {
		return nil, fmt.Errorf("parsing created timestamp: %w", err)
	}

	s.Updated, err = time.Parse(time.RFC3339, updatedStr)
	if err != nil {
		return nil, fmt.Errorf("parsing updated timestamp: %w", err)
	}

	return &s, nil
}

// SetRepoSiteConfig inserts or replaces the site configuration for a repo.
func SetRepoSiteConfig(e Execer, repoAt, branch, dir string, isIndex bool) error {
	isIndexInt := 0
	if isIndex {
		isIndexInt = 1
	}

	_, err := e.Exec(`
		insert into repo_sites (repo_at, branch, dir, is_index, updated)
		values (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		on conflict(repo_at) do update set
			branch   = excluded.branch,
			dir      = excluded.dir,
			is_index = excluded.is_index,
			updated  = excluded.updated
	`, repoAt, branch, dir, isIndexInt)
	return err
}

// DeleteRepoSiteConfig removes the site configuration for a repo.
func DeleteRepoSiteConfig(e Execer, repoAt string) error {
	_, err := e.Exec(`delete from repo_sites where repo_at = ?`, repoAt)
	return err
}
