package pages

import (
	"context"
	"database/sql"
	"fmt"
	"path"
	"strconv"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func (p *Pages) MakeCommentUrl(ctx context.Context, uri syntax.ATURI) (string, error) {
	comment, err := db.GetComment(p.db, orm.FilterEq("at_uri", uri))
	if err != nil {
		return "", fmt.Errorf("failed to get comment: %w", err)
	}
	subjectUri := syntax.ATURI(comment.Subject.Uri)
	switch subjectUri.Collection() {
	case tangled.RepoIssueNSID:
		issueUrl, err := p.MakeIssueUrl(ctx, subjectUri)
		if err != nil {
			return "", fmt.Errorf("failed to make issue url: %w", err)
		}
		return issueUrl + fmt.Sprintf("#comment-%s", comment.Rkey), nil
	case tangled.RepoPullNSID:
		if comment.PullRoundIdx == nil {
			return "", fmt.Errorf("comment.pullRoundIdx is missing")
		}
		pullUrl, err := p.MakePullUrl(ctx, subjectUri, *comment.PullRoundIdx)
		if err != nil {
			return "", fmt.Errorf("failed to make pull url: %w", err)
		}
		return pullUrl + fmt.Sprintf("#comment-%s", comment.Rkey), nil
	case tangled.StringNSID:
		stringUrl, err := p.MakeStringUrl(ctx, subjectUri)
		if err != nil {
			return "", fmt.Errorf("failed to make string url: %w", err)
		}
		return stringUrl + fmt.Sprintf("#comment-%s", comment.Rkey), nil
	default:
		return "", fmt.Errorf("unknown subject collection '%s'", subjectUri.Collection())
	}
}

func (p *Pages) MakeIssueUrl(ctx context.Context, uri syntax.ATURI) (string, error) {
	issue, err := func(uri syntax.ATURI) (*models.Issue, error) {
		issues, err := db.GetIssues(p.db, orm.FilterEq("at_uri", uri))
		if err != nil {
			return nil, err
		}
		if len(issues) != 1 {
			return nil, sql.ErrNoRows
		}
		return &issues[0], nil
	}(uri)
	if err != nil {
		return "", fmt.Errorf("failed to get issue: %w", err)
	}
	repoUrl, err := p.makeRepoUrlInner(ctx, issue.Repo)
	if err != nil {
		return "", fmt.Errorf("failed to make repo url: %w", err)
	}
	return path.Join(repoUrl, "issues", strconv.Itoa(issue.IssueId)), nil
}

func (p *Pages) MakePullUrl(ctx context.Context, uri syntax.ATURI, roundIdx int) (string, error) {
	pull, err := db.GetPull(p.db, orm.FilterEq("at_uri", uri))
	if err != nil {
		return "", fmt.Errorf("failed to get pull: %w", err)
	}
	repoUrl, err := p.makeRepoUrlInner(ctx, pull.Repo)
	if err != nil {
		return "", fmt.Errorf("failed to make repo url: %w", err)
	}
	return path.Join(repoUrl, "pulls", strconv.Itoa(pull.PullId), "rounds", strconv.Itoa(roundIdx)), nil
}

func (p *Pages) makeRepoUrlInner(ctx context.Context, repo *models.Repo) (string, error) {
	owner := repo.Did
	ownerIdentity, err := p.resolver.Directory().LookupDID(ctx, syntax.DID(repo.Did))
	if err == nil && !ownerIdentity.Handle.IsInvalidHandle() {
		owner = ownerIdentity.Handle.String()
	}
	return path.Join("/", owner, repo.Slug()), nil
}

func (p *Pages) MakeStringUrl(ctx context.Context, uri syntax.ATURI) (string, error) {
	string_, err := func(uri syntax.ATURI) (*models.String, error) {
		strings, err := db.GetStrings(p.db, 1, orm.FilterEq("at_uri", uri))
		if err != nil {
			return nil, err
		}
		if len(strings) != 1 {
			return nil, sql.ErrNoRows
		}
		return &strings[0], nil
	}(uri)
	if err != nil {
		return "", fmt.Errorf("failed to get string: %w", err)
	}

	owner := string_.Did.String()
	ownerIdentity, err := p.resolver.Directory().LookupDID(ctx, string_.Did)
	if err == nil && !ownerIdentity.Handle.IsInvalidHandle() {
		owner = ownerIdentity.Handle.String()
	}
	return path.Join("/strings", owner, string_.Rkey), nil
}
