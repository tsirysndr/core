package repo

import (
	"encoding/base64"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/reporesolver"
	xrpcclient "tangled.org/core/appview/xrpcclient"
	"tangled.org/core/types"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
	"github.com/go-git/go-git/v5/plumbing"
)

// the content can be one of the following:
//
// - code      : text |          | raw
// - markup    : text | rendered | raw
// - svg       : text | rendered | raw
// - png       :      | rendered | raw
// - video     :      | rendered | raw
// - submodule :      | rendered |
// - rest      :      |          |
func (rp *Repo) Blob(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoBlob")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)

	filePath := chi.URLParam(r, "*")
	filePath, _ = url.PathUnescape(filePath)

	scheme := "http"
	if !rp.config.Core.Dev {
		scheme = "https"
	}
	host := fmt.Sprintf("%s://%s", scheme, f.Knot)
	xrpcc := &indigoxrpc.Client{
		Host: host,
	}
	resp, err := tangled.RepoBlob(r.Context(), xrpcc, filePath, false, ref, f.RepoIdentifier())
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.blob", "err", xrpcerr)
		rp.pages.Error503(w)
		return
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)

	// Use XRPC response directly instead of converting to internal types
	var breadcrumbs [][]string
	breadcrumbs = append(breadcrumbs, []string{f.Name, fmt.Sprintf("/%s/tree/%s", ownerSlashRepo, url.PathEscape(ref))})
	if filePath != "" {
		for idx, elem := range strings.Split(filePath, "/") {
			breadcrumbs = append(breadcrumbs, []string{elem, fmt.Sprintf("%s/%s", breadcrumbs[idx][1], url.PathEscape(elem))})
		}
	}

	// Create the blob view
	blobView := NewBlobView(resp, rp.config, f, ref, filePath, r.URL.Query())

	user := rp.oauth.GetMultiAccountUser(r)

	// Get email to DID mapping for commit author
	var emails []string
	if resp.LastCommit != nil && resp.LastCommit.Author != nil {
		emails = append(emails, resp.LastCommit.Author.Email)
	}
	emailToDidMap, err := db.GetEmailToDid(rp.db, emails, true)
	if err != nil {
		l.Error("failed to get email to did mapping", "err", err)
		emailToDidMap = make(map[string]string)
	}

	var lastCommitInfo *types.LastCommitInfo
	if resp.LastCommit != nil {
		when, _ := time.Parse(time.RFC3339, resp.LastCommit.When)
		lastCommitInfo = &types.LastCommitInfo{
			Hash:    plumbing.NewHash(resp.LastCommit.Hash),
			Message: resp.LastCommit.Message,
			When:    when,
		}
		if resp.LastCommit.Author != nil {
			lastCommitInfo.Author.Name = resp.LastCommit.Author.Name
			lastCommitInfo.Author.Email = resp.LastCommit.Author.Email
			lastCommitInfo.Author.When, _ = time.Parse(time.RFC3339, resp.LastCommit.Author.When)
		}
	}

	rp.pages.RepoBlob(w, pages.RepoBlobParams{
		LoggedInUser:    user,
		RepoInfo:        rp.repoResolver.GetRepoInfo(r, user),
		BreadCrumbs:     breadcrumbs,
		BlobView:        blobView,
		EmailToDid:      emailToDidMap,
		LastCommitInfo:  lastCommitInfo,
		RepoBlob_Output: resp,
	})
}

func (rp *Repo) RepoBlobRaw(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoBlobRaw")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)

	filePath := chi.URLParam(r, "*")
	filePath, _ = url.PathUnescape(filePath)

	scheme := "http"
	if !rp.config.Core.Dev {
		scheme = "https"
	}
	repo := f.RepoIdentifier()
	baseURL := &url.URL{
		Scheme: scheme,
		Host:   f.Knot,
		Path:   "/xrpc/sh.tangled.repo.blob",
	}
	query := baseURL.Query()
	query.Set("repo", repo)
	query.Set("ref", ref)
	query.Set("path", filePath)
	query.Set("raw", "true")
	baseURL.RawQuery = query.Encode()
	blobURL := baseURL.String()
	req, err := http.NewRequest("GET", blobURL, nil)
	if err != nil {
		l.Error("failed to create request", "err", err)
		return
	}

	// forward the If-None-Match header
	if clientETag := r.Header.Get("If-None-Match"); clientETag != "" {
		req.Header.Set("If-None-Match", clientETag)
	}
	client := &http.Client{}

	resp, err := client.Do(req)
	if err != nil {
		l.Error("failed to reach knotserver", "err", err)
		rp.pages.Error503(w)
		return
	}

	defer resp.Body.Close()

	// forward 304 not modified
	if resp.StatusCode == http.StatusNotModified {
		w.WriteHeader(http.StatusNotModified)
		return
	}

	if resp.StatusCode != http.StatusOK {
		l.Error("knotserver returned non-OK status for raw blob", "url", blobURL, "statuscode", resp.StatusCode)
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.WriteHeader(resp.StatusCode)
		return
	}

	contentType := resp.Header.Get("Content-Type")
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		l.Error("error reading response body from knotserver", "err", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	if strings.HasPrefix(contentType, "text/") || isTextualMimeType(contentType) {
		// serve all textual content as text/plain
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.Write(body)
	} else if strings.HasPrefix(contentType, "image/") || strings.HasPrefix(contentType, "video/") {
		// serve images and videos with their original content type
		w.Header().Set("Content-Type", contentType)
		w.Write(body)
	} else {
		w.WriteHeader(http.StatusUnsupportedMediaType)
		w.Write([]byte("unsupported content type"))
		return
	}
}

// NewBlobView creates a BlobView from the XRPC response
func NewBlobView(resp *tangled.RepoBlob_Output, config *config.Config, repo *models.Repo, ref, filePath string, queryParams url.Values) models.BlobView {
	view := models.BlobView{
		Contents: "",
		Lines:    0,
	}

	// Set size
	if resp.Size != nil {
		view.SizeHint = uint64(*resp.Size)
	} else if resp.Content != nil {
		view.SizeHint = uint64(len(*resp.Content))
	}

	if resp.Submodule != nil {
		view.ContentType = models.BlobContentTypeSubmodule
		view.HasRenderedView = true
		view.ContentSrc = resp.Submodule.Url
		return view
	}

	// Determine if binary
	if resp.IsBinary != nil && *resp.IsBinary {
		view.ContentSrc = generateBlobURL(config, repo, ref, filePath)
		ext := strings.ToLower(filepath.Ext(resp.Path))

		switch ext {
		case ".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".jxl", ".heic", ".heif":
			view.ContentType = models.BlobContentTypeImage
			view.HasRawView = true
			view.HasRenderedView = true
			view.ShowingRendered = true

		case ".svg":
			view.ContentType = models.BlobContentTypeSvg
			view.HasRawView = true
			view.HasTextView = true
			view.HasRenderedView = true
			view.ShowingRendered = queryParams.Get("code") != "true"
			if resp.Content != nil {
				bytes, _ := base64.StdEncoding.DecodeString(*resp.Content)
				view.Contents = string(bytes)
				view.Lines = countLines(view.Contents)
			}

		case ".mp4", ".webm", ".ogg", ".mov", ".avi":
			view.ContentType = models.BlobContentTypeVideo
			view.HasRawView = true
			view.HasRenderedView = true
			view.ShowingRendered = true
		}

		return view
	}

	// otherwise, we are dealing with text content
	view.HasRawView = true
	view.HasTextView = true

	if resp.Content != nil {
		view.Contents = *resp.Content
		view.Lines = countLines(view.Contents)
	}

	// with text, we may be dealing with markdown
	format := markup.GetFormat(resp.Path)
	if format == markup.FormatMarkdown {
		view.ContentType = models.BlobContentTypeMarkup
		view.HasRenderedView = true
		view.ShowingRendered = queryParams.Get("code") != "true"
	}

	return view
}

func generateBlobURL(config *config.Config, repo *models.Repo, ref, filePath string) string {
	scheme := "http"
	if !config.Core.Dev {
		scheme = "https"
	}

	repoName := repo.RepoIdentifier()
	baseURL := &url.URL{
		Scheme: scheme,
		Host:   repo.Knot,
		Path:   "/xrpc/sh.tangled.repo.blob",
	}
	query := baseURL.Query()
	query.Set("repo", repoName)
	query.Set("ref", ref)
	query.Set("path", filePath)
	query.Set("raw", "true")
	baseURL.RawQuery = query.Encode()
	blobURL := baseURL.String()

	if !config.Core.Dev {
		return markup.GenerateCamoURL(config.Camo.Host, config.Camo.SharedSecret, blobURL)
	}
	return blobURL
}

func isTextualMimeType(mimeType string) bool {
	textualTypes := []string{
		"application/json",
		"application/xml",
		"application/yaml",
		"application/x-yaml",
		"application/toml",
		"application/javascript",
		"application/ecmascript",
		"message/",
	}
	return slices.Contains(textualTypes, mimeType)
}

// TODO: dedup with strings
func countLines(content string) int {
	if content == "" {
		return 0
	}

	count := strings.Count(content, "\n")

	if !strings.HasSuffix(content, "\n") {
		count++
	}

	return count
}
