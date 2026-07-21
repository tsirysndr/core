package xrpc

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"path"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/sites"
	"tangled.org/core/appview/state/userutil"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) SiteGetDomainClaim(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteGetDomainClaim")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	claim, err := db.GetActiveDomainClaimForDid(x.DB, did)
	if err != nil {
		l.Error("failed to get domain claim", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	out := &tangled.TempSiteGetDomainClaim_Output{}
	if claim != nil {
		out.Domain = &claim.Domain
	}
	x.writeJSON(w, out)
}

func (x *Xrpc) SiteClaimDomain(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteClaimDomain")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempSiteClaimDomain_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	subdomain := strings.TrimSpace(input.Subdomain)
	if len(subdomain) < 4 {
		writeError(w, xrpcErrorTag("InvalidSubdomain", "subdomain must be at least 4 characters long"), http.StatusBadRequest)
		return
	}
	if !userutil.IsValidSubdomain(subdomain) {
		writeError(w, xrpcErrorTag("InvalidSubdomain", "use only lowercase letters, digits, and hyphens; cannot start or end with a hyphen"), http.StatusBadRequest)
		return
	}
	if userutil.HasSlur(subdomain) {
		writeError(w, xrpcErrorTag("InvalidSubdomain", "that subdomain is not allowed"), http.StatusBadRequest)
		return
	}

	sitesDomain := x.Config.Sites.Domain
	if subdomain == sitesDomain {
		writeError(w, xrpcErrorTag("InvalidSubdomain", "cannot claim the root domain"), http.StatusBadRequest)
		return
	}
	fullDomain := subdomain + "." + sitesDomain

	if err := db.ClaimDomain(x.DB, did, fullDomain); err != nil {
		switch {
		case errors.Is(err, db.ErrDomainTaken):
			writeError(w, xrpcErrorTag("DomainTaken", err.Error()), http.StatusConflict)
		case errors.Is(err, db.ErrDomainCooldown):
			writeError(w, xrpcErrorTag("DomainCooldown", err.Error()), http.StatusConflict)
		case errors.Is(err, db.ErrAlreadyClaimed):
			writeError(w, xrpcErrorTag("AlreadyClaimed", err.Error()), http.StatusConflict)
		default:
			l.Error("claiming domain", "err", err)
			writeError(w, errInternal, http.StatusInternalServerError)
		}
		return
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) SiteReleaseDomain(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteReleaseDomain")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempSiteReleaseDomain_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	domain := strings.TrimSpace(input.Domain)
	if domain == "" {
		writeError(w, badRequestError("domain cannot be empty"), http.StatusBadRequest)
		return
	}

	// a tngl.sh handle's sites domain is auto-claimed at signup and handle-bound
	if isTngl, err := x.isTnglHandle(r.Context(), did); err != nil {
		l.Error("resolving identity", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	} else if isTngl {
		writeError(w, xrpcErrorTag("HandleBoundDomain", "your tngl.sh domain is tied to your handle and cannot be released"), http.StatusBadRequest)
		return
	}

	if err := db.ReleaseDomain(x.DB, did, domain); err != nil {
		l.Error("releasing domain", "err", err)
		writeError(w, xrpcErrorTag("DomainNotFound", "unable to release domain; ensure it belongs to your account"), http.StatusNotFound)
		return
	}

	// clean up all site data for this did asynchronously
	if x.Cloudflare != nil && x.Cloudflare.Enabled() {
		siteConfigs, err := db.GetRepoSiteConfigsForDid(x.DB, did)
		if err != nil {
			l.Error("fetching site configs for cleanup", "err", err)
		}
		if err := db.DeleteRepoSiteConfigsForDid(x.DB, did); err != nil {
			l.Error("deleting site configs from db", "err", err)
		}

		go func() {
			ctx := context.Background()
			for _, sc := range siteConfigs {
				if err := sites.Delete(ctx, x.Cloudflare, did, sc.RepoRkey); err != nil {
					l.Error("R2 delete failed", "did", did, "repo", sc.RepoRkey, "err", err)
				}
			}
			if err := sites.DeleteAllDomainMappings(ctx, x.Cloudflare, domain); err != nil {
				l.Error("KV delete failed", "domain", domain, "err", err)
			}
		}()
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) SiteGetRepoSiteConfig(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteGetRepoSiteConfig")

	repo, xerr, status := x.resolveOwnedRepo(r, r.URL.Query().Get("repoDid"))
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	config, err := db.GetRepoSiteConfig(x.DB, repo.RepoDid)
	if err != nil {
		l.Error("failed to get repo site config", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	out := &tangled.TempRepoGetSiteConfig_Output{}
	if config != nil {
		out.Config = &tangled.TempRepoGetSiteConfig_SiteConfig{
			Branch:  config.Branch,
			Dir:     config.Dir,
			IsIndex: config.IsIndex,
		}
	}
	x.writeJSON(w, out)
}

func (x *Xrpc) SiteUpdateRepoSiteConfig(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteUpdateRepoSiteConfig")

	var input tangled.TempRepoUpdateSiteConfig_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	branch := strings.TrimSpace(input.Branch)
	if branch == "" {
		writeError(w, badRequestError("branch cannot be empty"), http.StatusBadRequest)
		return
	}

	dir := strings.TrimSpace(input.Dir)
	if dir == "" {
		dir = "/"
	}
	dir = path.Clean("/" + dir)
	if dir != "/" && strings.Contains(dir, "..") {
		writeError(w, badRequestError("invalid directory path"), http.StatusBadRequest)
		return
	}

	isIndex := input.IsIndex != nil && *input.IsIndex

	// check the claim before persisting, so a failed call leaves no state
	ownerClaim, _ := db.GetActiveDomainClaimForDid(x.DB, repo.Did)
	if ownerClaim == nil {
		writeError(w, xrpcErrorTag("NoDomainClaim", "the account does not have an active domain claim"), http.StatusBadRequest)
		return
	}

	if err := db.SetRepoSiteConfig(x.DB, repo.RepoDid, branch, dir, isIndex); err != nil {
		l.Error("failed to save site config", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	if x.Cloudflare != nil && x.Cloudflare.Enabled() {
		go x.deploySite(repo, branch, dir, isIndex, ownerClaim.Domain)
	} else {
		l.Warn("cloudflare integration disabled; site won't be deployed", "repo", repo.RepoIdentifier())
	}

	w.WriteHeader(http.StatusOK)
}

func (x *Xrpc) SiteDisableRepoSite(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SiteDisableRepoSite")

	var input tangled.TempRepoDisableSite_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	repo, xerr, status := x.resolveOwnedRepo(r, input.RepoDid)
	if xerr != nil {
		writeError(w, *xerr, status)
		return
	}

	existingConfig, _ := db.GetRepoSiteConfig(x.DB, repo.RepoDid)
	if existingConfig == nil {
		writeError(w, xrpcErrorTag("SiteNotFound", "no site configuration exists for this repository"), http.StatusNotFound)
		return
	}

	if err := db.DeleteRepoSiteConfig(x.DB, repo.RepoDid); err != nil {
		l.Error("failed to delete site config", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	if x.Cloudflare != nil && x.Cloudflare.Enabled() {
		ownerClaim, _ := db.GetActiveDomainClaimForDid(x.DB, repo.Did)
		go func() {
			ctx := context.Background()
			if err := sites.Delete(ctx, x.Cloudflare, repo.Did, repo.Rkey); err != nil {
				l.Error("R2 delete failed", "repo", repo.RepoIdentifier(), "err", err)
			}
			if ownerClaim != nil {
				if err := sites.DeleteDomainMapping(ctx, x.Cloudflare, ownerClaim.Domain, repo.Name); err != nil {
					l.Error("KV delete failed", "domain", ownerClaim.Domain, "err", err)
				}
			}
		}()
	}

	w.WriteHeader(http.StatusOK)
}

// deploySite syncs a repo's site to r2 and writes the domain mapping, mirroring
// the appview's SaveRepoSiteConfig deploy path
func (x *Xrpc) deploySite(repo *models.Repo, branch, dir string, isIndex bool, domain string) {
	l := x.Logger.With("handler", "deploySite", "repo", repo.RepoIdentifier())
	ctx := context.Background()

	deploy := &models.SiteDeploy{
		RepoDid: syntax.DID(repo.RepoDid),
		Branch:  branch,
		Dir:     dir,
		Trigger: models.SiteDeployTriggerConfigChange,
	}

	deployErr := sites.Deploy(ctx, x.Cloudflare, x.Config, repo, branch, dir)
	if deployErr != nil {
		l.Error("initial R2 sync failed", "err", deployErr)
		deploy.Status = models.SiteDeployStatusFailure
		deploy.Error = deployErr.Error()
	} else {
		deploy.Status = models.SiteDeployStatusSuccess
	}

	if err := db.AddSiteDeploy(x.DB, deploy); err != nil {
		l.Error("failed to record deploy", "err", err)
	}

	if deployErr == nil {
		if err := sites.PutDomainMapping(ctx, x.Cloudflare, domain, repo.Did, repo.Name, repo.Rkey, isIndex); err != nil {
			l.Error("KV write failed", "domain", domain, "err", err)
		}
	}
}

// isTnglHandle reports whether the account's handle sits under the PDS user
// domain (e.g. *.tngl.sh). Such users have a handle-bound sites domain that was
// auto-claimed at signup and must not be released.
func (x *Xrpc) isTnglHandle(ctx context.Context, did string) (bool, error) {
	ident, err := x.IdResolver.ResolveIdent(ctx, did)
	if err != nil {
		return false, err
	}
	return strings.HasSuffix(ident.Handle.String(), x.Config.Pds.UserDomain), nil
}
