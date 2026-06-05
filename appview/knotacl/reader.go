package knotacl

import (
	"context"
	"fmt"
	"slices"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
)

type reader interface {
	repoPerms(ctx context.Context, repo *models.Repo, userDid string) ([]string, error)
	knotMembers(ctx context.Context, host string) []string
	collaborators(ctx context.Context, repo *models.Repo) []pages.Collaborator
	isRepoCreateAllowed(ctx context.Context, host, userDid string) bool
	isKnotMember(ctx context.Context, host, userDid string) bool
}

type legacyReader struct {
	enforcer *rbac.Enforcer
}

func (r *legacyReader) repoPerms(ctx context.Context, repo *models.Repo, userDid string) ([]string, error) {
	return r.enforcer.GetPermissionsInRepo(userDid, repo.Knot, repo.RepoIdentifier()), nil
}

func (r *legacyReader) knotMembers(ctx context.Context, host string) []string {
	members, err := r.enforcer.GetUserByRole("server:member", host)
	if err != nil {
		return nil
	}
	return members
}

func (r *legacyReader) collaborators(ctx context.Context, repo *models.Repo) []pages.Collaborator {
	policies, err := r.enforcer.E.GetImplicitUsersForResourceByDomain(repo.RepoIdentifier(), repo.Knot)
	if err != nil {
		return nil
	}
	return filterMap(policies, func(p []string) (pages.Collaborator, bool) {
		// currently only two roles: owner and member
		switch p[3] {
		case "repo:owner":
			return pages.Collaborator{Did: p[0], Role: "owner"}, true
		case "repo:collaborator":
			return pages.Collaborator{Did: p[0], Role: "collaborator"}, true
		default:
			return pages.Collaborator{}, false
		}
	})
}

func (r *legacyReader) isRepoCreateAllowed(ctx context.Context, host, userDid string) bool {
	ok, err := r.enforcer.IsRepoCreateAllowed(userDid, host)
	return err == nil && ok
}

func (r *legacyReader) isKnotMember(ctx context.Context, host, userDid string) bool {
	knots, err := r.enforcer.GetKnotsForUser(userDid)
	return err == nil && slices.Contains(knots, host)
}

type nativeReader struct {
	client *cache
	execer db.Execer
}

func (r *nativeReader) repoPerms(ctx context.Context, repo *models.Repo, userDid string) ([]string, error) {
	if userDid == repo.Did {
		return ownerPermissions(), nil
	}
	var perms []string
	if r.isRegisteredOwner(ctx, repo.Knot, userDid) {
		perms = serverOwnerRepoPermissions()
	}
	collabs, err := r.client.GetRepoCollaborators(ctx, repo.Knot, repo.RepoDid)
	if err != nil {
		return dedup(perms), fmt.Errorf("%w: %v", ErrKnotUnreachable, err)
	}
	if slices.Contains(collabs, userDid) {
		perms = append(perms, collaboratorPermissions()...)
	}
	return dedup(perms), nil
}

func (r *nativeReader) knotMembers(ctx context.Context, host string) []string {
	members, err := r.client.GetKnotMembers(ctx, host)
	if err != nil {
		return dedup(r.registeredOwners(ctx, host))
	}
	return dedup(append(members, r.registeredOwners(ctx, host)...))
}

func (r *nativeReader) collaborators(ctx context.Context, repo *models.Repo) []pages.Collaborator {
	owner := pages.Collaborator{Did: repo.Did, Role: "owner"}
	collabs, err := r.client.GetRepoCollaborators(ctx, repo.Knot, repo.RepoDid)
	if err != nil {
		return []pages.Collaborator{owner}
	}
	rows := filterMap(collabs, func(d string) (pages.Collaborator, bool) {
		if d == repo.Did {
			return pages.Collaborator{}, false
		}
		return pages.Collaborator{Did: d, Role: "collaborator"}, true
	})
	return append([]pages.Collaborator{owner}, rows...)
}

func (r *nativeReader) isRepoCreateAllowed(ctx context.Context, host, userDid string) bool {
	members, err := r.client.GetKnotMembers(ctx, host)
	if err == nil && slices.Contains(members, userDid) {
		return true
	}
	return r.isRegisteredOwner(ctx, host, userDid)
}

func (r *nativeReader) isKnotMember(ctx context.Context, host, userDid string) bool {
	return slices.Contains(r.knotMembers(ctx, host), userDid)
}

func (r *nativeReader) registeredOwners(ctx context.Context, host string) []string {
	key := "r\x00" + host
	if memo := memoFrom(ctx); memo != nil {
		if v, ok := memo.get(key); ok {
			return slices.Clone(v)
		}
	}
	regs, err := db.GetRegistrations(r.execer, orm.FilterEq("domain", host))
	if err != nil {
		return nil
	}
	owners := filterMap(regs, func(reg models.Registration) (string, bool) {
		return reg.ByDid, reg.Registered != nil
	})
	if memo := memoFrom(ctx); memo != nil {
		memo.put(key, slices.Clone(owners))
	}
	return owners
}

func (r *nativeReader) isRegisteredOwner(ctx context.Context, host, userDid string) bool {
	return slices.Contains(r.registeredOwners(ctx, host), userDid)
}
