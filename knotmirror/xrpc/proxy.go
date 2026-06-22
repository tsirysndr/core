package xrpc

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"io"
	"maps"
	"net/http"
	"net/url"
	"path"
	"strings"

	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/models"
)

var mirrorToKnotNSID = map[string]string{
	tangled.GitTempListBranchesNSID:  tangled.RepoBranchesNSID,
	tangled.GitTempListTagsNSID:      tangled.RepoTagsNSID,
	tangled.GitTempListCommitsNSID:   tangled.RepoLogNSID,
	tangled.GitTempGetTreeNSID:       tangled.RepoTreeNSID,
	tangled.GitTempGetBranchNSID:     tangled.RepoBranchNSID,
	tangled.GitTempGetTagNSID:        tangled.RepoTagNSID,
	tangled.GitTempGetArchiveNSID:    tangled.RepoArchiveNSID,
	tangled.GitTempListLanguagesNSID: tangled.RepoLanguagesNSID,
	tangled.GitTempGetBlobNSID:       tangled.RepoBlobNSID,
}

var hopByHopHeaders = map[string]bool{
	"Connection":          true,
	"Keep-Alive":          true,
	"Transfer-Encoding":   true,
	"Te":                  true,
	"Trailer":             true,
	"Upgrade":             true,
	"Proxy-Authorization": true,
	"Proxy-Authenticate":  true,
}

type knotInfo struct {
	baseURL        string
	repoIdentifier string
}

// validateKnotURL ensures a knot base URL is safe to proxy to.
// It rejects URLs with path components, query strings, or fragments
// that could be used for path injection.
func validateKnotURL(raw string) (string, error) {
	u, err := url.Parse(raw)
	if err != nil {
		return "", fmt.Errorf("invalid knot URL: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return "", errors.New("knot URL must use http or https scheme")
	}
	if u.Path != "" && u.Path != "/" {
		return "", fmt.Errorf("knot URL must not contain a path: %q", raw)
	}
	if u.RawQuery != "" || u.Fragment != "" {
		return "", fmt.Errorf("knot URL must not contain query or fragment: %q", raw)
	}
	if u.User != nil {
		return "", fmt.Errorf("knot URL must not contain userinfo: %q", raw)
	}
	// Strip trailing slash for consistent formatting
	return strings.TrimRight(u.String(), "/"), nil
}

func (x *Xrpc) resolveKnot(ctx context.Context, repoDid syntax.DID) (*knotInfo, error) {
	if repo, err := db.GetRepoByRepoDid(ctx, x.db, repoDid); err == nil && repo != nil {
		knotURL := repo.KnotDomain
		if !strings.Contains(repo.KnotDomain, "://") {
			if host, _ := db.GetHost(ctx, x.db, repo.KnotDomain); host != nil {
				knotURL = host.URL()
			} else {
				x.logger.Warn("repo is from unknown knot")
				if x.cfg.KnotUseSSL {
					knotURL = "https://" + knotURL
				} else {
					knotURL = "http://" + knotURL
				}
			}
		}
		knotURL, err = validateKnotURL(knotURL)
		if err != nil {
			return nil, err
		}
		return &knotInfo{baseURL: knotURL, repoIdentifier: repo.RepoIdentifier()}, nil
	}

	ident, err := x.resolver.ResolveIdent(ctx, repoDid.String())
	if err != nil {
		return nil, fmt.Errorf("resolving repoDid %s: %w", repoDid, err)
	}
	knotURL, err := validateKnotURL(ident.GetServiceEndpoint("atproto_pds"))
	if err != nil {
		return nil, fmt.Errorf("repoDid %s: %w", repoDid, err)
	}

	xrpcc := &indigoxrpc.Client{Host: knotURL, Client: x.httpClient}
	out, err := tangled.RepoDescribeRepo(ctx, xrpcc, repoDid.String())
	if err != nil {
		x.logger.Warn("describeRepo failed; serving without metadata upsert", "knot", knotURL, "repo", repoDid, "err", err)
		return &knotInfo{baseURL: knotURL, repoIdentifier: repoDid.String()}, nil
	}
	if out.RepoDid != repoDid.String() {
		return nil, fmt.Errorf("knot %s returned mismatched repoDid: got %q, want %q", knotURL, out.RepoDid, repoDid)
	}
	ownerDid, err := syntax.ParseDID(out.OwnerDid)
	if err != nil {
		return nil, fmt.Errorf("describeRepo on %s returned invalid ownerDid %q: %w", knotURL, out.OwnerDid, err)
	}
	rkey, err := syntax.ParseRecordKey(out.Rkey)
	if err != nil {
		return nil, fmt.Errorf("describeRepo on %s returned invalid rkey %q: %w", knotURL, out.Rkey, err)
	}

	go func() {
		pending := &models.Repo{
			Did:        ownerDid,
			Rkey:       rkey,
			Name:       string(rkey),
			KnotDomain: knotURL,
			RepoDid:    repoDid,
			State:      models.RepoStatePending,
		}
		if err := db.UpsertRepo(context.Background(), x.db, pending); err != nil {
			x.logger.Error("failed to upsert repo after directory resolution", "err", err)
		}
	}()

	return &knotInfo{baseURL: knotURL, repoIdentifier: repoDid.String()}, nil
}

func (x *Xrpc) proxyToKnot(w http.ResponseWriter, r *http.Request, repoDid syntax.DID) bool {
	mirrorNSID := strings.TrimPrefix(r.URL.Path, "/xrpc/")
	knotNSID, ok := mirrorToKnotNSID[mirrorNSID]
	if !ok {
		return false
	}

	knot, err := x.resolveKnot(r.Context(), repoDid)
	if err != nil {
		x.logger.Warn("proxy: failed to resolve knot", "repo", repoDid, "err", err)
		return false
	}

	params := make(url.Values)
	maps.Copy(params, r.URL.Query())
	params.Set("repo", knot.repoIdentifier)

	target := fmt.Sprintf("%s/xrpc/%s?%s", knot.baseURL, knotNSID, params.Encode())

	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, target, nil)
	if err != nil {
		x.logger.Warn("proxy: failed to build request", "target", target, "err", err)
		return false
	}

	resp, err := x.httpClient.Do(req)
	if err != nil {
		x.logger.Warn("proxy: knot request failed", "target", target, "err", err)
		return false
	}
	defer resp.Body.Close()

	for k, vv := range resp.Header {
		if hopByHopHeaders[k] {
			continue
		}
		for _, v := range vv {
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
	if _, err := io.Copy(w, resp.Body); err != nil {
		x.logger.Warn("proxy: response copy interrupted", "target", target, "err", err)
	}

	x.logger.Info("proxy: served from knot", "repo", repoDid, "knot", knot.baseURL, "status", resp.StatusCode)
	return true
}

func (x *Xrpc) forwardSuspended(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		repoDid, err := syntax.ParseDID(r.URL.Query().Get("repo"))
		if err != nil {
			next.ServeHTTP(w, r)
			return
		}

		repo, err := db.GetRepoByRepoDid(r.Context(), x.db, repoDid)
		if err != nil || repo == nil || repo.State != models.RepoStateSuspended {
			next.ServeHTTP(w, r)
			return
		}

		nsid := strings.TrimPrefix(r.URL.Path, "/xrpc/")
		switch nsid {
		case tangled.GitTempGetEntryNSID:
			x.serveSuspendedEntry(w, r, repoDid)
		case tangled.GitTempGetBlobNSID:
			q := r.URL.Query()
			q.Set("raw", "true")
			r.URL.RawQuery = q.Encode()
			x.forwardOrFail(w, r, repoDid)
		default:
			if _, ok := mirrorToKnotNSID[nsid]; !ok {
				next.ServeHTTP(w, r)
				return
			}
			x.forwardOrFail(w, r, repoDid)
		}
	})
}

func (x *Xrpc) forwardOrFail(w http.ResponseWriter, r *http.Request, repoDid syntax.DID) {
	if x.proxyToKnot(w, r, repoDid) {
		return
	}
	writeJson(w, http.StatusBadGateway, atclient.ErrorBody{Name: "BadGateway", Message: "failed to reach knot for suspended repo"})
}

func (x *Xrpc) serveSuspendedEntry(w http.ResponseWriter, r *http.Request, repoDid syntax.DID) {
	ref := cmp.Or(r.URL.Query().Get("ref"), "HEAD")
	filePath := r.URL.Query().Get("path")
	if filePath == "" {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "missing path parameter"})
		return
	}

	knot, err := x.resolveKnot(r.Context(), repoDid)
	if err != nil {
		x.logger.Warn("suspended entry: failed to resolve knot", "repo", repoDid, "err", err)
		writeJson(w, http.StatusBadGateway, atclient.ErrorBody{Name: "BadGateway", Message: "failed to resolve knot for suspended repo"})
		return
	}

	client := &indigoxrpc.Client{Host: knot.baseURL, Client: x.httpClient}
	out, err := tangled.RepoBlob(r.Context(), client, filePath, false, ref, knot.repoIdentifier)
	if err != nil {
		x.logger.Warn("suspended entry: knot repo.blob failed", "repo", repoDid, "err", err)
		writeJson(w, http.StatusBadGateway, atclient.ErrorBody{Name: "BadGateway", Message: "failed to read entry from knot"})
		return
	}

	mode := filemode.Regular
	if out.Submodule != nil {
		mode = filemode.Submodule
	}

	writeJson(w, http.StatusOK, tangled.GitTempGetEntry_Output{
		Name:       path.Base(filePath),
		Mode:       mode.String(),
		Size:       derefInt64(out.Size),
		LastCommit: suspendedLastCommit(out.LastCommit),
		Submodule:  suspendedSubmodule(out.Submodule),
	})
}

func suspendedLastCommit(c *tangled.RepoBlob_LastCommit) *tangled.GitTempDefs_Commit {
	if c == nil || c.Author == nil {
		return nil
	}
	sig := suspendedSignature(c.Author)
	hash := c.Hash
	return &tangled.GitTempDefs_Commit{
		Author:    sig,
		Committer: sig,
		Hash:      &hash,
		Message:   c.Message,
	}
}

func suspendedSignature(s *tangled.RepoBlob_Signature) *tangled.GitTempDefs_Signature {
	if s == nil {
		return nil
	}
	return &tangled.GitTempDefs_Signature{
		Name:  s.Name,
		Email: s.Email,
		When:  s.When,
	}
}

func suspendedSubmodule(s *tangled.RepoBlob_Submodule) *tangled.GitTempDefs_Submodule {
	if s == nil {
		return nil
	}
	return &tangled.GitTempDefs_Submodule{
		Name:   s.Name,
		Url:    s.Url,
		Branch: s.Branch,
	}
}

func derefInt64(v *int64) int64 {
	if v == nil {
		return 0
	}
	return *v
}
