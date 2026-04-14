package xrpc

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"

	"github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/hostutil"
	"tangled.org/core/knotmirror/models"
)

func (x *Xrpc) RequestCrawl(w http.ResponseWriter, r *http.Request) {
	var input tangled.SyncRequestCrawl_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: "failed to decode json body"})
		return
	}

	ctx := r.Context()

	l := x.logger.With("input", input)

	hostname, noSSL, err := hostutil.ParseHostname(input.Hostname)
	if err != nil {
		l.Error("invalid hostname", "err", err)
		writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("hostname field empty or invalid: %s", input.Hostname)})
		return
	}

	// TODO: check if host is Knot with knot.describeServer

	// store given repoAt to db
	// this will allow knotmirror to ingest repo creation event bypassing tap.
	// this step won't be needed once we introduce did-for-repo
	// TODO(boltless): remove this section
	if input.EnsureRepo != nil {
		repoAt, err := syntax.ParseATURI(*input.EnsureRepo)
		if err != nil {
			l.Error("invalid repo at-uri", "err", err)
			writeJson(w, http.StatusBadRequest, atclient.ErrorBody{Name: "BadRequest", Message: fmt.Sprintf("repo parameter invalid: %s", *input.EnsureRepo)})
			return
		}
		owner, err := x.resolver.ResolveIdent(ctx, repoAt.Authority().String())
		if err != nil || owner.Handle.IsInvalidHandle() {
			l.Error("failed to resolve ident", "err", err, "owner", repoAt.Authority().String())
			writeErr(w, fmt.Errorf("failed to resolve repo owner"))
			return
		}
		xrpcc := xrpc.Client{Host: owner.PDSEndpoint()}
		out, err := atproto.RepoGetRecord(ctx, &xrpcc, "", tangled.RepoNSID, repoAt.Authority().String(), repoAt.RecordKey().String())
		if err != nil {
			l.Error("failed to get repo record", "err", err, "repo", repoAt)
			writeErr(w, fmt.Errorf("failed to get repo record"))
			return
		}
		record := out.Value.Val.(*tangled.Repo)

		knotUrl := record.Knot
		if !strings.Contains(record.Knot, "://") {
			if noSSL {
				knotUrl = "http://" + knotUrl
			} else {
				knotUrl = "https://" + knotUrl
			}
		}

		if record.RepoDid == nil || *record.RepoDid == "" {
			l.Warn("dropping repo crawl request without repo_did", "did", owner.DID, "rkey", repoAt.RecordKey())
			writeErr(w, fmt.Errorf("repo record missing repo_did"))
			return
		}

		repo := &models.Repo{
			Did:        owner.DID,
			Rkey:       repoAt.RecordKey(),
			Cid:        (*syntax.CID)(out.Cid),
			Name:       repoAt.RecordKey().String(),
			KnotDomain: knotUrl,
			RepoDid:    syntax.DID(*record.RepoDid),
			State:      models.RepoStatePending,
			ErrorMsg:   "",
			RetryAfter: 0,
			RetryCount: 0,
		}

		x.logger.Debug("requestCrawl: upserting repo with knot", "knot", repo.KnotDomain)
		if err := db.UpsertRepo(ctx, x.db, repo); err != nil {
			l.Error("failed to upsert repo", "err", err)
			writeErr(w, err)
			return
		}
	}

	// subscribe to requested host
	if !x.ks.CheckIfSubscribed(hostname) {
		if err := x.ks.SubscribeHost(ctx, hostname, noSSL); err != nil {
			// TODO(boltless): return HostBanned on banned hosts
			l.Error("failed to subscribe host", "err", err)
			writeErr(w, err)
			return
		}
	}

	w.WriteHeader(http.StatusOK)
}
