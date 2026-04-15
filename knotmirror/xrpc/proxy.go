package xrpc

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
)

var mirrorToKnotNSID = map[string]string{
	tangled.GitTempListBranchesNSID:  tangled.RepoBranchesNSID,
	tangled.GitTempListTagsNSID:      tangled.RepoTagsNSID,
	tangled.GitTempListCommitsNSID:   tangled.RepoLogNSID,
	tangled.GitTempGetTreeNSID:       tangled.RepoTreeNSID,
	tangled.GitTempGetBranchNSID:     tangled.RepoBranchNSID,
	tangled.GitTempGetBlobNSID:       tangled.RepoBlobNSID,
	tangled.GitTempGetTagNSID:        tangled.RepoTagNSID,
	tangled.GitTempGetArchiveNSID:    tangled.RepoArchiveNSID,
	tangled.GitTempListLanguagesNSID: tangled.RepoLanguagesNSID,
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
	baseURL      string
	didSlashRepo string
}

func (x *Xrpc) resolveKnot(ctx context.Context, repoAt syntax.ATURI) (*knotInfo, error) {
	repo, err := db.GetRepoByAtUri(ctx, x.db, repoAt)
	if err == nil && repo != nil {
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
		return &knotInfo{baseURL: knotURL, didSlashRepo: repo.DidSlashRepo()}, nil
	}

	owner, err := x.resolver.ResolveIdent(ctx, repoAt.Authority().String())
	if err != nil {
		return nil, fmt.Errorf("resolving repo owner: %w", err)
	}

	xrpcc := indigoxrpc.Client{Host: owner.PDSEndpoint()}
	out, err := atproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
	if err != nil {
		return nil, fmt.Errorf("fetching repo record from PDS: %w", err)
	}

	record := out.Value.Val.(*tangled.Repo)
	knotURL := record.Knot
	if !strings.Contains(record.Knot, "://") {
		if host, _ := db.GetHost(ctx, x.db, record.Knot); host != nil {
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

	return &knotInfo{
		baseURL:      knotURL,
		didSlashRepo: fmt.Sprintf("%s/%s", owner.DID, record.Name),
	}, nil
}

func (x *Xrpc) proxyToKnot(w http.ResponseWriter, r *http.Request, repoAt syntax.ATURI) bool {
	mirrorNSID := strings.TrimPrefix(r.URL.Path, "/xrpc/")
	knotNSID, ok := mirrorToKnotNSID[mirrorNSID]
	if !ok {
		return false
	}

	knot, err := x.resolveKnot(r.Context(), repoAt)
	if err != nil {
		x.logger.Warn("proxy: failed to resolve knot", "repo", repoAt, "err", err)
		return false
	}

	params := make(url.Values)
	for k, v := range r.URL.Query() {
		params[k] = v
	}
	params.Set("repo", knot.didSlashRepo)

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

	x.logger.Info("proxy: served from knot", "repo", repoAt, "knot", knot.baseURL, "status", resp.StatusCode)
	return true
}
