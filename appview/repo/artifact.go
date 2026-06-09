package repo

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/orm"
	"tangled.org/core/tid"
	"tangled.org/core/types"
	"tangled.org/core/xrpc"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/dustin/go-humanize"
	"github.com/go-chi/chi/v5"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/ipfs/go-cid"
)

// TODO: proper statuses here on early exit
func (rp *Repo) AttachArtifact(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "AttachArtifact")

	user := rp.oauth.GetMultiAccountUser(r)
	tagParam := chi.URLParam(r, "tag")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		rp.pages.Notice(w, "upload", "failed to upload artifact, error in repo resolution")
		return
	}

	tag, err := rp.resolveTag(r.Context(), f, tagParam)
	if err != nil {
		l.Error("failed to resolve tag", "err", err)
		rp.pages.Notice(w, "upload", "failed to upload artifact, error in tag resolution")
		return
	}

	file, header, err := r.FormFile("artifact")
	if err != nil {
		l.Error("failed to upload artifact", "err", err)
		rp.pages.Notice(w, "upload", "failed to upload artifact")
		return
	}
	defer file.Close()

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		rp.pages.Notice(w, "upload", "failed to get authorized client")
		return
	}

	uploadBlobResp, err := xrpc.RepoUploadBlob(r.Context(), client, file, header.Header.Get("Content-Type"))
	if err != nil {
		l.Error("failed to upload blob", "err", err)
		rp.pages.Notice(w, "upload", "Failed to upload blob to your PDS. Try again later.")
		return
	}

	l.Info("uploaded blob", "size", humanize.Bytes(uint64(uploadBlobResp.Blob.Size)), "blobRef", uploadBlobResp.Blob.Ref.String())

	rkey := tid.TID()
	createdAt := time.Now()

	putRecordResp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoArtifactNSID,
		Repo:       user.Did,
		Rkey:       rkey,
		Record: &lexutil.LexiconTypeDecoder{
			Val: repoArtifactRecord(f, uploadBlobResp.Blob, createdAt, header.Filename, tag.Tag.Hash[:]),
		},
	})
	if err != nil {
		l.Error("failed to create record", "err", err)
		rp.pages.Notice(w, "upload", "Failed to create artifact record. Try again later.")
		return
	}

	l.Debug("created record for blob", "aturi", putRecordResp.Uri)

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start tx")
		rp.pages.Notice(w, "upload", "Failed to create artifact. Try again later.")
		return
	}
	defer tx.Rollback()

	artifact := models.Artifact{
		Did:       user.Did,
		Rkey:      rkey,
		RepoDid:   syntax.DID(f.RepoDid),
		Tag:       tag.Tag.Hash,
		CreatedAt: createdAt,
		BlobCid:   cid.Cid(uploadBlobResp.Blob.Ref),
		Name:      header.Filename,
		Size:      uint64(uploadBlobResp.Blob.Size),
		MimeType:  uploadBlobResp.Blob.MimeType,
	}

	err = db.AddArtifact(tx, artifact)
	if err != nil {
		l.Error("failed to add artifact record to db", "err", err)
		rp.pages.Notice(w, "upload", "Failed to create artifact. Try again later.")
		return
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to add artifact record to db")
		rp.pages.Notice(w, "upload", "Failed to create artifact. Try again later.")
		return
	}

	rp.pages.RepoArtifactFragment(w, pages.RepoArtifactParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Artifact:   artifact,
	})
}

func (rp *Repo) DownloadArtifact(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DownloadArtifact")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		http.Error(w, "failed to resolve repo", http.StatusInternalServerError)
		return
	}

	tagParam := chi.URLParam(r, "tag")
	filename := chi.URLParam(r, "file")

	tag, err := rp.resolveTag(r.Context(), f, tagParam)
	if err != nil {
		l.Error("failed to resolve tag", "err", err)
		rp.pages.Notice(w, "upload", "failed to upload artifact, error in tag resolution")
		return
	}

	artifacts, err := db.GetArtifact(
		rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("tag", tag.Tag.Hash[:]),
		orm.FilterEq("name", filename),
	)
	if err != nil {
		l.Error("failed to get artifacts", "err", err)
		http.Error(w, "failed to get artifact", http.StatusInternalServerError)
		return
	}

	if len(artifacts) != 1 {
		l.Error("too many or too few artifacts found")
		http.Error(w, "artifact not found", http.StatusNotFound)
		return
	}

	artifact := artifacts[0]

	ownerId, err := rp.idResolver.ResolveIdent(r.Context(), f.Did)
	if err != nil {
		l.Error("failed to resolve repo owner did", "did", f.Did, "err", err)
		http.Error(w, "repository owner not found", http.StatusNotFound)
		return
	}

	ownerPds := ownerId.PDSEndpoint()
	url, _ := url.Parse(fmt.Sprintf("%s/xrpc/com.atproto.sync.getBlob", ownerPds))
	q := url.Query()
	q.Set("cid", artifact.BlobCid.String())
	q.Set("did", artifact.Did)
	url.RawQuery = q.Encode()

	req, err := http.NewRequest(http.MethodGet, url.String(), nil)
	if err != nil {
		l.Error("failed to create request", "err", err)
		http.Error(w, "failed to create request", http.StatusInternalServerError)
		return
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		l.Error("failed to make request", "err", err)
		http.Error(w, "failed to make request to PDS", http.StatusInternalServerError)
		return
	}
	defer resp.Body.Close()

	// copy status code and relevant headers from upstream response
	w.WriteHeader(resp.StatusCode)
	for key, values := range resp.Header {
		for _, v := range values {
			w.Header().Add(key, v)
		}
	}

	// stream the body directly to the client
	if _, err := io.Copy(w, resp.Body); err != nil {
		l.Error("error streaming response to client:", "err", err)
	}
}

// TODO: proper statuses here on early exit
func (rp *Repo) DeleteArtifact(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DeleteArtifact")

	user := rp.oauth.GetMultiAccountUser(r)
	tagParam := chi.URLParam(r, "tag")
	filename := chi.URLParam(r, "file")
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	client, _ := rp.oauth.AuthorizedClient(r)

	tag := plumbing.NewHash(tagParam)

	artifacts, err := db.GetArtifact(
		rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("tag", tag[:]),
		orm.FilterEq("name", filename),
	)
	if err != nil {
		l.Error("failed to get artifacts", "err", err)
		rp.pages.Notice(w, "remove", "Failed to delete artifact. Try again later.")
		return
	}
	if len(artifacts) != 1 {
		rp.pages.Notice(w, "remove", "Unable to find artifact.")
		return
	}

	artifact := artifacts[0]

	if user.Did != artifact.Did {
		l.Error("user not authorized to delete artifact", "err", err)
		rp.pages.Notice(w, "remove", "Unauthorized deletion of artifact.")
		return
	}

	_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.RepoArtifactNSID,
		Repo:       user.Did,
		Rkey:       artifact.Rkey,
	})
	if err != nil {
		l.Error("failed to get blob from pds", "err", err)
		rp.pages.Notice(w, "remove", "Failed to remove blob from PDS.")
		return
	}

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start tx")
		rp.pages.Notice(w, "remove", "Failed to delete artifact. Try again later.")
		return
	}
	defer tx.Rollback()

	err = db.DeleteArtifact(tx,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("tag", artifact.Tag[:]),
		orm.FilterEq("name", filename),
	)
	if err != nil {
		l.Error("failed to remove artifact record from db", "err", err)
		rp.pages.Notice(w, "remove", "Failed to delete artifact. Try again later.")
		return
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to remove artifact record from db")
		rp.pages.Notice(w, "remove", "Failed to delete artifact. Try again later.")
		return
	}

	l.Info("successfully deleted artifact", "tag", tagParam, "file", filename)

	w.Write([]byte{})
}

func (rp *Repo) resolveTag(ctx context.Context, f *models.Repo, tagParam string) (*types.TagReference, error) {
	l := rp.logger.With("handler", "resolveTag")

	tagParam, err := url.QueryUnescape(tagParam)
	if err != nil {
		return nil, err
	}

	xrpcc := &indigoxrpc.Client{Host: rp.config.KnotMirror.Url}

	xrpcBytes, err := tangled.GitTempListTags(ctx, xrpcc, "", 0, f.RepoDid)
	if err != nil {
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.tags", "xrpcerr", xrpcerr, "err", err)
			return nil, xrpcerr
		}
		l.Error("failed to reach knotserver", "err", err)
		return nil, err
	}

	var result types.RepoTagsResponse
	if err := json.Unmarshal(xrpcBytes, &result); err != nil {
		l.Error("failed to decode XRPC tags response", "err", err)
		return nil, err
	}

	var tag *types.TagReference
	for _, t := range result.Tags {
		if t.Tag != nil {
			if t.Reference.Name == tagParam || t.Reference.Hash == tagParam {
				tag = t
			}
		}
	}

	if tag == nil {
		return nil, fmt.Errorf("invalid tag, only annotated tags are supported for artifacts")
	}

	if tag.Tag.Target.IsZero() {
		return nil, fmt.Errorf("invalid tag, only annotated tags are supported for artifacts")
	}

	return tag, nil
}

func repoArtifactRecord(f *models.Repo, blob *lexutil.LexBlob, createdAt time.Time, name string, tag []byte) *tangled.RepoArtifact {
	rec := &tangled.RepoArtifact{
		Artifact:  blob,
		CreatedAt: createdAt.Format(time.RFC3339),
		Name:      name,
		Tag:       tag,
	}
	s := f.RepoAt().String()
	rec.Repo = &s
	if f.RepoDid != "" {
		rec.RepoDid = &f.RepoDid
	}
	return rec
}
