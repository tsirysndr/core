package knotacl

import (
	"context"
	"errors"
	"log/slog"
	"slices"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"golang.org/x/sync/errgroup"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/consts"
	"tangled.org/core/rbac"
)

var ErrKnotUnreachable = errors.New("knot unreachable")

const (
	pickerFanoutBudget      = 3 * time.Second
	pickerFanoutConcurrency = 16
)

type Service struct {
	dev bool
	log *slog.Logger
	leg *legacyReader
	nat *nativeReader
}

func NewService(enforcer *rbac.Enforcer, store *db.DB, dev bool, logger *slog.Logger) *Service {
	return &Service{
		dev: dev,
		log: logger,
		leg: &legacyReader{enforcer: enforcer},
		nat: &nativeReader{client: newRoster(store, NewClient(dev, logger), reconcileTTL, nil, logger), execer: store},
	}
}

func (s *Service) reader(ctx context.Context, host string) reader {
	if knotcompat.KnotHasCapability(ctx, host, s.dev, consts.CapKnotACL) {
		return s.nat
	}
	return s.leg
}

func (s *Service) RolesInRepo(ctx context.Context, repo *models.Repo, userDid string) repoinfo.RolesInRepo {
	return repoinfo.RolesInRepo{Roles: s.repoPerms(ctx, repo, userDid)}
}

func (s *Service) HasRepoPermission(ctx context.Context, repo *models.Repo, userDid, perm string) bool {
	return slices.Contains(s.repoPerms(ctx, repo, userDid), perm)
}

func (s *Service) HasRepoPermissionErr(ctx context.Context, repo *models.Repo, userDid, perm string) (bool, error) {
	perms, err := s.repoPermsErr(ctx, repo, userDid)
	if err != nil {
		return false, err
	}
	return slices.Contains(perms, perm), nil
}

func (s *Service) IsRepoCreateAllowed(ctx context.Context, host, userDid string) bool {
	return s.reader(ctx, host).isRepoCreateAllowed(ctx, host, userDid)
}

func (s *Service) KnotMembers(ctx context.Context, host string) []string {
	return s.reader(ctx, host).knotMembers(ctx, host)
}

func (s *Service) Collaborators(ctx context.Context, repo *models.Repo) []pages.Collaborator {
	return s.reader(ctx, repo.Knot).collaborators(ctx, repo)
}

func (s *Service) IsKnotMember(ctx context.Context, host, userDid string) bool {
	return s.reader(ctx, host).isKnotMember(ctx, host, userDid)
}

func (s *Service) InvalidateMembers(host string) {
	s.nat.client.InvalidateMembers(host)
}

func (s *Service) InvalidateCollaborators(host, repoDid string) {
	s.nat.client.InvalidateCollaborators(host, repoDid)
}

func (s *Service) AddKnotMember(host string, subject syntax.DID, cursor Cursor) error {
	return s.nat.client.AddKnotMember(host, subject, cursor)
}

func (s *Service) RemoveKnotMember(host string, subject syntax.DID, cursor Cursor) error {
	return s.nat.client.RemoveKnotMember(host, subject, cursor)
}

func (s *Service) AddCollaborator(repoDid, subject syntax.DID, cursor Cursor) error {
	return s.nat.client.AddCollaborator(repoDid, subject, cursor)
}

func (s *Service) RemoveCollaborator(repoDid, subject syntax.DID, cursor Cursor) error {
	return s.nat.client.RemoveCollaborator(repoDid, subject, cursor)
}

func (s *Service) KnotsForUser(ctx context.Context, userDid string) []string {
	legacyKnots, err := s.leg.enforcer.GetKnotsForUser(userDid)
	if err != nil {
		s.log.Error("knotsForUser: enforcer lookup failed, returning a partial list", "did", userDid, "err", err)
	}

	regs, err := db.GetRegistrations(s.nat.execer)
	if err != nil {
		s.log.Error("knotsForUser: registrations lookup failed, skipping native knots", "did", userDid, "err", err)
	}
	domains := dedup(filterMap(regs, func(r models.Registration) (string, bool) {
		return r.Domain, r.Registered != nil
	}))
	owned := filterMap(regs, func(r models.Registration) (string, bool) {
		return r.Domain, r.Registered != nil && r.ByDid == userDid
	})
	nativeMember := s.nativeMemberships(ctx, domains, userDid)

	all := make([]string, 0, len(legacyKnots)+len(owned)+len(nativeMember))
	all = append(all, legacyKnots...)
	all = append(all, owned...)
	all = append(all, nativeMember...)
	return dedup(all)
}

func (s *Service) nativeMemberships(ctx context.Context, domains []string, userDid string) []string {
	ctx, cancel := context.WithTimeout(ctx, pickerFanoutBudget)
	defer cancel()

	g, gctx := errgroup.WithContext(ctx)
	g.SetLimit(pickerFanoutConcurrency)

	var mu sync.Mutex
	var hits []string

	for _, host := range domains {
		g.Go(func() error {
			if !knotcompat.KnotHasCapability(gctx, host, s.dev, consts.CapKnotACL) {
				return nil
			}
			members, err := s.nat.client.GetKnotMembers(gctx, host)
			if err != nil || !slices.Contains(members, userDid) {
				return nil
			}
			mu.Lock()
			hits = append(hits, host)
			mu.Unlock()
			return nil
		})
	}
	_ = g.Wait()
	return hits
}

func (s *Service) repoPerms(ctx context.Context, repo *models.Repo, userDid string) []string {
	perms, _ := s.repoPermsErr(ctx, repo, userDid)
	return perms
}

func (s *Service) repoPermsErr(ctx context.Context, repo *models.Repo, userDid string) ([]string, error) {
	return s.reader(ctx, repo.Knot).repoPerms(ctx, repo, userDid)
}

func filterMap[T, U any](items []T, f func(T) (U, bool)) []U {
	var out []U
	for _, it := range items {
		if u, ok := f(it); ok {
			out = append(out, u)
		}
	}
	return out
}
