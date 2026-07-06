package middleware

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"path"
	"slices"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/state/userutil"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
)

type Middleware struct {
	oauth        *oauth.OAuth
	db           *db.DB
	enforcer     *rbac.Enforcer
	acl          *knotacl.Service
	repoResolver *reporesolver.RepoResolver
	idResolver   *idresolver.Resolver
	pages        *pages.Pages
	rdb          *cache.Cache
	logger       *slog.Logger
}

func New(oauth *oauth.OAuth, db *db.DB, enforcer *rbac.Enforcer, acl *knotacl.Service, repoResolver *reporesolver.RepoResolver, idResolver *idresolver.Resolver, pages *pages.Pages, rdb *cache.Cache, logger *slog.Logger) Middleware {
	return Middleware{
		oauth:        oauth,
		db:           db,
		enforcer:     enforcer,
		acl:          acl,
		repoResolver: repoResolver,
		idResolver:   idResolver,
		pages:        pages,
		rdb:          rdb,
		logger:       logger,
	}
}

type middlewareFunc func(http.Handler) http.Handler

func AuthMiddleware(o *oauth.OAuth) middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			returnURL := "/"
			if u, err := url.Parse(r.Header.Get("Referer")); err == nil {
				returnURL = u.RequestURI()
			}

			loginURL := fmt.Sprintf("/login?return_url=%s", url.QueryEscape(returnURL))

			redirectFunc := func(w http.ResponseWriter, r *http.Request) {
				http.Redirect(w, r, loginURL, http.StatusTemporaryRedirect)
			}
			if r.Header.Get("HX-Request") == "true" {
				redirectFunc = func(w http.ResponseWriter, _ *http.Request) {
					w.Header().Set("HX-Redirect", loginURL)
					w.WriteHeader(http.StatusOK)
				}
			}

			sess, err := o.ResumeSession(r)
			if err != nil {
				slog.Default().Warn("failed to resume session, redirecting", "err", err, "url", r.URL.String())
				redirectFunc(w, r)
				return
			}

			if sess == nil {
				slog.Default().Warn("session is nil, redirecting")
				redirectFunc(w, r)
				return
			}

			next.ServeHTTP(w, r)
		})
	}
}

func (m *Middleware) InjectBaseParams(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		user := m.oauth.GetMultiAccountUser(r)
		bp := pages.BaseParams{
			LoggedInUser: user,
		}
		if user != nil {
			if focusing, _ := db.GetFocusStatus(m.db, user.Did); focusing {
				if item, _ := db.GetNextFocusItem(m.db, user.Did); item != nil {
					count, _ := db.CountFocusNotifs(m.db, user.Did)
					bp.FocusParams = pages.FocusParams{
						Focusing:            true,
						FocusLink:           item.URL(m.idResolver),
						FocusNotificationID: item.ID,
						CurrentPath:         r.URL.Path,
						FocusCount:          int(count),
					}
				} else {
					// queue exhausted — auto-exit focus mode
					_ = db.EndFocus(m.db, user.Did)
				}
			}
		}
		ctx := pages.BaseParamsIntoContext(r.Context(), bp)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

func Paginate(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		page := pagination.FirstPage()

		offsetVal := r.URL.Query().Get("offset")
		if offsetVal != "" {
			offset, err := strconv.Atoi(offsetVal)
			if err != nil {
				slog.Default().Warn("invalid offset", "value", offsetVal)
			} else {
				page.Offset = offset
			}
		}

		limitVal := r.URL.Query().Get("limit")
		if limitVal != "" {
			limit, err := strconv.Atoi(limitVal)
			if err != nil {
				slog.Default().Warn("invalid limit", "value", limitVal)
			} else {
				page.Limit = limit
			}
		}

		ctx := pagination.IntoContext(r.Context(), page)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

func (mw Middleware) knotRoleMiddleware(group string) middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			l := mw.logger.With("middleware", "knotRoleMiddleware")
			// requires auth also
			actor := mw.oauth.GetMultiAccountUser(r)
			if actor == nil {
				// we need a logged in user
				l.Warn("not logged in, redirecting")
				http.Error(w, "Forbidden", http.StatusUnauthorized)
				return
			}
			domain := chi.URLParam(r, "domain")
			if domain == "" {
				http.Error(w, "malformed url", http.StatusBadRequest)
				return
			}

			ok, err := mw.enforcer.E.HasGroupingPolicy(actor.Did, group, domain)
			if err != nil || !ok {
				l.Warn("permission denied", "did", actor.Did, "group", group, "domain", domain)
				http.Error(w, "Forbidden", http.StatusUnauthorized)
				return
			}

			next.ServeHTTP(w, r)
		})
	}
}

func (mw Middleware) KnotOwner() middlewareFunc {
	return mw.knotRoleMiddleware("server:owner")
}

func (mw Middleware) RepoPermissionMiddleware(requiredPerm string) middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			l := mw.logger.With("middleware", "RepoPermissionMiddleware")
			// requires auth also
			actor := mw.oauth.GetMultiAccountUser(r)
			if actor == nil {
				// we need a logged in user
				l.Warn("not logged in, redirecting")
				http.Error(w, "Forbidden", http.StatusUnauthorized)
				return
			}
			f, err := mw.repoResolver.Resolve(r)
			if err != nil {
				http.Error(w, "malformed url", http.StatusBadRequest)
				return
			}

			if !mw.acl.HasRepoPermission(r.Context(), f, actor.Did, requiredPerm) {
				l.Warn("permission denied", "did", actor.Did, "perm", requiredPerm, "repo", f.RepoIdentifier())
				http.Error(w, "Forbidden", http.StatusUnauthorized)
				return
			}

			next.ServeHTTP(w, r)
		})
	}
}

func (mw Middleware) ResolveIdent() middlewareFunc {
	excluded := []string{"favicon.ico"}

	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
			origSeg := chi.URLParam(req, "user")
			didOrHandle := strings.TrimPrefix(origSeg, "@")
			didOrHandle = strings.TrimSuffix(didOrHandle, ".keys")

			if slices.Contains(excluded, didOrHandle) {
				next.ServeHTTP(w, req)
				return
			}

			id, err := mw.idResolver.ResolveAtIdentifier(req.Context(), didOrHandle)
			if err != nil {
				if h, parseErr := syntax.ParseHandle(didOrHandle); parseErr == nil {
					if did := cache.LookupDidByPreferredHandle(req.Context(), mw.rdb, mw.db, h); did != "" {
						id, err = mw.idResolver.ResolveAtIdentifier(req.Context(), did)
					}
				}
			}
			if err != nil {
				mw.logger.Error("failed to resolve did/handle", "didOrHandle", didOrHandle, "err", err)
				mw.pages.Error404(w)
				return
			}

			if req.Method == http.MethodGet && !userutil.IsDid(didOrHandle) {
				if pref := cache.LookupPreferredHandle(req.Context(), mw.rdb, mw.db, id.DID.String()); pref != "" && didOrHandle != pref {
					rest := strings.TrimPrefix(req.URL.Path, "/"+origSeg)
					target := "/" + pref + rest
					if req.URL.RawQuery != "" {
						target += "?" + req.URL.RawQuery
					}
					http.Redirect(w, req, target, http.StatusFound)
					return
				}
			}

			ctx := context.WithValue(req.Context(), "resolvedId", *id)

			next.ServeHTTP(w, req.WithContext(ctx))
		})
	}
}

func (mw Middleware) ResolveRepo() middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
			l := mw.logger.With("middleware", "ResolveRepo")
			repoName := strings.TrimSuffix(chi.URLParam(req, "repo"), ".git")
			rkey := strings.ToLower(repoName)

			id, ok := req.Context().Value("resolvedId").(identity.Identity)
			if !ok {
				l.Error("malformed middleware")
				w.WriteHeader(http.StatusInternalServerError)
				return
			}

			repo, isRename := resolveRepoForOwner(mw.db, id.DID.String(), repoName, rkey, l)
			if repo == nil {
				w.WriteHeader(http.StatusNotFound)
				mw.pages.ErrorKnot404(w)
				return
			}
			if isRename {
				handle := id.Handle.String()
				if id.Handle.IsInvalidHandle() || handle == "" {
					handle = id.DID.String()
				}
				canonical := reporesolver.CanonicalRepoPath(handle, repo)
				if path.Join(chi.URLParam(req, "user"), repoName) != canonical {
					target := reporesolver.CanonicalRedirectTarget(req, canonical)
					http.Redirect(w, req, target, http.StatusMovedPermanently)
					return
				}
			}

			ctx := context.WithValue(req.Context(), "repo", repo)
			next.ServeHTTP(w, req.WithContext(ctx))
		})
	}
}

func resolveRepoForOwner(d db.Execer, ownerDid, repoName, rkey string, l *slog.Logger) (*models.Repo, bool) {
	repo, err := db.GetRepo(d, orm.FilterEq("did", ownerDid), orm.FilterEq("rkey", rkey))
	if err == nil {
		return repo, false
	}
	if !errors.Is(err, sql.ErrNoRows) {
		l.Error("failed to resolve repo by rkey", "err", err)
		return nil, false
	}

	hint, hintErr := db.LookupRepoRename(d, ownerDid, rkey)
	if hintErr != nil && !errors.Is(hintErr, sql.ErrNoRows) {
		l.Error("failed to lookup repo rename hint", "err", hintErr)
	}
	if hint != nil {
		return hint, true
	}

	nameRepos, nameErr := db.GetRepos(d, orm.FilterEq("did", ownerDid), orm.FilterEq("name", repoName))
	if nameErr != nil {
		l.Error("failed to resolve repo by name", "err", nameErr)
		return nil, false
	}
	if len(nameRepos) == 1 {
		return &nameRepos[0], false
	}
	return nil, false
}

func (mw Middleware) CanonicalizeRepoURL() middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
			if req.Method != http.MethodGet && req.Method != http.MethodHead {
				next.ServeHTTP(w, req)
				return
			}
			id, idOk := req.Context().Value("resolvedId").(identity.Identity)
			repo, repoOk := req.Context().Value("repo").(*models.Repo)
			if !idOk || !repoOk || id.Handle.IsInvalidHandle() {
				next.ServeHTTP(w, req)
				return
			}
			handle := id.Handle.String()
			if handle == "" {
				next.ServeHTTP(w, req)
				return
			}
			canonical := reporesolver.CanonicalRepoPath(handle, repo)
			urlUser := chi.URLParam(req, "user")
			urlRepo := strings.TrimSuffix(chi.URLParam(req, "repo"), ".git")
			if urlUser+"/"+urlRepo == canonical {
				next.ServeHTTP(w, req)
				return
			}

			http.Redirect(w, req, reporesolver.CanonicalRedirectTarget(req, canonical), http.StatusFound)
		})
	}
}

// middleware that is tacked on top of /{user}/{repo}/pulls/{pull}
func (mw Middleware) ResolvePull() middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			l := mw.logger.With("middleware", "ResolvePull")
			f, err := mw.repoResolver.Resolve(r)
			if err != nil {
				l.Error("failed to fully resolve repo", "err", err)
				w.WriteHeader(http.StatusNotFound)
				mw.pages.ErrorKnot404(w)
				return
			}

			prId := chi.URLParam(r, "pull")
			prIdInt, err := strconv.Atoi(prId)
			if err != nil {
				l.Error("failed to parse pr id", "err", err)
				mw.pages.Error404(w)
				return
			}

			pr, err := db.GetPull(mw.db, orm.FilterEq("repo_did", f.RepoDid), orm.FilterEq("pull_id", prIdInt))
			if err != nil {
				l.Error("failed to get pull and comments", "err", err)
				mw.pages.Error404(w)
				return
			}

			ctx := context.WithValue(r.Context(), "pull", pr)

			stack, err := db.GetStack(mw.db, pr.AtUri())
			if err != nil {
				l.Error("failed to get stack", "err", err)
				mw.pages.Error404(w)
				return
			}

			ctx = context.WithValue(ctx, "stack", stack)

			next.ServeHTTP(w, r.WithContext(ctx))
		})
	}
}

// middleware that is tacked on top of /{user}/{repo}/issues/{issue}
func (mw Middleware) ResolveIssue(next http.Handler) http.Handler {
	l := mw.logger.With("middleware", "ResolveIssue")
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		f, err := mw.repoResolver.Resolve(r)
		if err != nil {
			l.Error("failed to fully resolve repo", "err", err)
			w.WriteHeader(http.StatusNotFound)
			mw.pages.ErrorKnot404(w)
			return
		}

		issueIdStr := chi.URLParam(r, "issue")
		issueId, err := strconv.Atoi(issueIdStr)
		if err != nil {
			l.Error("failed to fully resolve issue ID", "err", err)
			mw.pages.Error404(w)
			return
		}

		issue, err := db.GetIssue(mw.db, f.RepoDid, issueId)
		if err != nil {
			l.Error("failed to get issues", "err", err)
			mw.pages.Error404(w)
			return
		}

		ctx := context.WithValue(r.Context(), "issue", issue)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

// this should serve the go-import meta tag even if the path is technically
// a 404 like tangled.sh/oppi.li/go-git/v5
//
// we're keeping the tangled.sh go-import tag too to maintain backward
// compatibility for modules that still point there. they will be redirected
// to fetch source from tangled.org
func (mw Middleware) GoImport() middlewareFunc {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			l := mw.logger.With("middleware", "GoImport")
			f, err := mw.repoResolver.Resolve(r)
			if err != nil {
				l.Error("failed to fully resolve repo", "err", err)
				w.WriteHeader(http.StatusNotFound)
				mw.pages.ErrorKnot404(w)
				return
			}

			fullName := reporesolver.GetBaseRepoPath(r, f)

			if r.Header.Get("User-Agent") == "Go-http-client/1.1" {
				if r.URL.Query().Get("go-get") == "1" {
					modulePath := userutil.FlattenDid(fullName)
					if strings.Contains(modulePath, ":") {
						modulePath = userutil.FlattenDid(f.Did) + "/" + f.Rkey
					}
					tags := []string{
						fmt.Sprintf(`<meta name="go-import" content="tangled.sh/%s git https://tangled.sh/%s"/>`, modulePath, fullName),
						fmt.Sprintf(`<meta name="go-import" content="tangled.org/%s git https://tangled.org/%s"/>`, modulePath, fullName),
					}
					if f.RepoDid != "" {
						stable := userutil.FlattenDid(f.RepoDid)
						if stable != modulePath {
							tags = append(tags,
								fmt.Sprintf(`<meta name="go-import" content="tangled.sh/%s git https://tangled.sh/%s"/>`, stable, f.RepoDid),
								fmt.Sprintf(`<meta name="go-import" content="tangled.org/%s git https://tangled.org/%s"/>`, stable, f.RepoDid),
							)
						}
					}
					w.Header().Set("Content-Type", "text/html")
					w.Write([]byte(strings.Join(tags, "\n")))
					return
				}
			}

			next.ServeHTTP(w, r)
		})
	}
}
