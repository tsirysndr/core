package reporesolver

import (
	"fmt"
	"log"
	"net/http"
	"path"
	"regexp"
	"strings"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/rbac"
)

var (
	blobPattern = regexp.MustCompile(`blob/[^/]+/(.*)$`)
	treePattern = regexp.MustCompile(`tree/[^/]+/(.*)$`)
)

type RepoResolver struct {
	config   *config.Config
	enforcer *rbac.Enforcer
	execer   db.Execer
	rdb      *cache.Cache
}

func New(config *config.Config, enforcer *rbac.Enforcer, execer db.Execer, rdb *cache.Cache) *RepoResolver {
	return &RepoResolver{config: config, enforcer: enforcer, execer: execer, rdb: rdb}
}

// NOTE: this... should not even be here. the entire package will be removed in future refactor
func GetBaseRepoPath(r *http.Request, repo *models.Repo) string {
	if repo.RepoDid != "" {
		return repo.RepoDid
	}
	var (
		user = chi.URLParam(r, "user")
		name = chi.URLParam(r, "repo")
	)
	if user == "" || name == "" {
		return repo.RepoIdentifier()
	}
	return path.Join(user, name)
}

// TODO: move this out of `RepoResolver` struct
func (rr *RepoResolver) Resolve(r *http.Request) (*models.Repo, error) {
	repo, ok := r.Context().Value("repo").(*models.Repo)
	if !ok {
		log.Println("malformed middleware: `repo` not exist in context")
		return nil, fmt.Errorf("malformed middleware")
	}

	return repo, nil
}

// 1. [x] replace `RepoInfo` to `reporesolver.GetRepoInfo(r *http.Request, repo, user)`
// 2. [x] remove `rr`, `CurrentDir`, `Ref` fields from `ResolvedRepo`
// 3. [x] remove `ResolvedRepo`
// 4. [ ] replace reporesolver to reposervice
func (rr *RepoResolver) GetRepoInfo(r *http.Request, user *oauth.MultiAccountUser) repoinfo.RepoInfo {
	ownerId, ook := r.Context().Value("resolvedId").(identity.Identity)
	repo, rok := r.Context().Value("repo").(*models.Repo)
	if !ook || !rok {
		log.Println("malformed request, failed to get repo from context")
	}

	// get dir/ref
	currentDir := extractCurrentDir(r.URL.EscapedPath())
	ref := chi.URLParam(r, "ref")

	repoDid := repo.RepoDid
	isStarred := false
	roles := repoinfo.RolesInRepo{}
	if user != nil {
		isStarred = db.GetStarStatus(rr.execer, user.Did, repoDid)
		roles.Roles = rr.enforcer.GetPermissionsInRepo(user.Did, repo.Knot, repo.RepoIdentifier())
	}

	stats := repo.RepoStats
	if stats == nil {
		starCount, starErr := db.GetStarCount(rr.execer, models.StarSubjectRepo, repoDid)
		if starErr != nil {
			log.Println("failed to get star count for ", repoDid)
		}
		issueCount, err := db.GetIssueCount(rr.execer, repoDid)
		if err != nil {
			log.Println("failed to get issue count for ", repoDid)
		}
		pullCount, err := db.GetPullCount(rr.execer, repoDid)
		if err != nil {
			log.Println("failed to get pull count for ", repoDid)
		}
		stats = &models.RepoStats{
			StarCount:  starCount,
			IssueCount: issueCount,
			PullCount:  pullCount,
		}
	}

	var sourceRepo *models.Repo
	var err error
	if repo.Source != "" {
		if strings.HasPrefix(repo.Source, "did:") {
			sourceRepo, err = db.GetRepoByDid(rr.execer, repo.Source)
		} else {
			sourceRepo, err = db.GetRepoByAtUri(rr.execer, repo.Source)
		}
		if err != nil {
			log.Println("failed to get source repo", err)
		}
	}

	ownerHandle := ownerId.Handle.String()
	if h := cache.LookupPreferredHandle(r.Context(), rr.rdb, rr.execer, ownerId.DID.String()); h != "" {
		ownerHandle = h
	}

	repoInfo := repoinfo.RepoInfo{
		// this is basically a models.Repo
		OwnerDid:    ownerId.DID.String(),
		OwnerHandle: ownerHandle,
		RepoDid:     repo.RepoDid,
		Name:        repo.Name,
		Rkey:        repo.Rkey,
		Description: repo.Description,
		Website:     repo.Website,
		Topics:      repo.Topics,
		Knot:        repo.Knot,
		Spindle:     repo.Spindle,
		Stats:       *stats,

		// fork repo upstream
		Source: sourceRepo,

		// page context
		CurrentDir: currentDir,
		Ref:        ref,

		// info related to the session
		IsStarred: isStarred,
		Roles:     roles,
	}

	return repoInfo
}

// extractCurrentDir gets the current directory for markdown link resolution.
// for blob paths, returns the parent dir. for tree paths, returns the path itself.
//
//	/@user/repo/blob/main/docs/README.md => docs
//	/@user/repo/tree/main/docs           => docs
func extractCurrentDir(fullPath string) string {
	fullPath = strings.TrimPrefix(fullPath, "/")

	if matches := blobPattern.FindStringSubmatch(fullPath); len(matches) > 1 {
		return path.Dir(matches[1])
	}

	if matches := treePattern.FindStringSubmatch(fullPath); len(matches) > 1 {
		dir := strings.TrimSuffix(matches[1], "/")
		if dir == "" {
			return "."
		}
		return dir
	}

	return "."
}
