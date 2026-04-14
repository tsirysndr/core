package middleware

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"slices"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/db"
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
	repoResolver *reporesolver.RepoResolver
	idResolver   *idresolver.Resolver
	pages        *pages.Pages
	rdb          *cache.Cache
	logger       *slog.Logger
}

func New(oauth *oauth.OAuth, db *db.DB, enforcer *rbac.Enforcer, repoResolver *reporesolver.RepoResolver, idResolver *idresolver.Resolver, pages *pages.Pages, rdb *cache.Cache, logger *slog.Logger) Middleware {
	return Middleware{
		oauth:        oauth,
		db:           db,
		enforcer:     enforcer,
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

			ok, err := mw.enforcer.E.Enforce(actor.Did, f.Knot, f.RepoIdentifier(), requiredPerm)
			if err != nil || !ok {
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
			repoName := chi.URLParam(req, "repo")
			repoName = strings.TrimSuffix(repoName, ".git")
			rkey := strings.ToLower(repoName)

			id, ok := req.Context().Value("resolvedId").(identity.Identity)
			if !ok {
				l.Error("malformed middleware")
				w.WriteHeader(http.StatusInternalServerError)
				return
			}

			repo, err := db.GetRepo(
				mw.db,
				orm.FilterEq("did", id.DID.String()),
				orm.FilterEq("rkey", rkey),
			)
			if err != nil {
				if !errors.Is(err, sql.ErrNoRows) {
					l.Error("failed to resolve repo", "err", err)
					http.Error(w, "internal server error", http.StatusInternalServerError)
					return
				}
				hint, hintErr := db.LookupRepoRename(mw.db, id.DID.String(), rkey)
				if hintErr != nil && !errors.Is(hintErr, sql.ErrNoRows) {
					l.Error("failed to lookup repo rename hint", "err", hintErr)
				}
				if hint != nil {
					parts := strings.SplitN(strings.TrimPrefix(req.URL.Path, "/"), "/", 3)
					target := "/" + parts[0] + "/" + hint.Rkey
					if len(parts) == 3 {
						target += "/" + parts[2]
					}
					if req.URL.RawQuery != "" {
						target += "?" + req.URL.RawQuery
					}
					http.Redirect(w, req, target, http.StatusMovedPermanently)
					return
				}
				nameRepos, nameErr := db.GetRepos(
					mw.db,
					orm.FilterEq("did", id.DID.String()),
					orm.FilterEq("name", repoName),
				)
				if nameErr == nil && len(nameRepos) == 1 && nameRepos[0].RepoDid != "" {
					nameRepo := &nameRepos[0]
					if _, tidErr := syntax.ParseTID(nameRepo.Rkey); tidErr == nil {
						ctx := context.WithValue(req.Context(), "repo", nameRepo)
						next.ServeHTTP(w, req.WithContext(ctx))
						return
					}
					parts := strings.SplitN(strings.TrimPrefix(req.URL.Path, "/"), "/", 3)
					target := "/" + nameRepo.RepoDid
					if len(parts) == 3 {
						target += "/" + parts[2]
					}
					if req.URL.RawQuery != "" {
						target += "?" + req.URL.RawQuery
					}
					http.Redirect(w, req, target, http.StatusFound)
					return
				}
				w.WriteHeader(http.StatusNotFound)
				mw.pages.ErrorKnot404(w)
				return
			}

			ctx := context.WithValue(req.Context(), "repo", repo)
			next.ServeHTTP(w, req.WithContext(ctx))
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
					html := fmt.Sprintf(
						`<meta name="go-import" content="tangled.sh/%s git https://tangled.sh/%s"/>
<meta name="go-import" content="tangled.org/%s git https://tangled.org/%s"/>`,
						modulePath, fullName,
						modulePath, fullName,
					)
					w.Header().Set("Content-Type", "text/html")
					w.Write([]byte(html))
					return
				}
			}

			next.ServeHTTP(w, r)
		})
	}
}
