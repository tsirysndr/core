package repo

import (
	"encoding/base64"
	"fmt"
	"io"
	"mime"
	"net/http"
	"net/url"
	"path/filepath"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/types"
	xrpcclient "tangled.org/core/xrpc/xrpcclient"

	"github.com/bluesky-social/indigo/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"
	enry "github.com/go-enry/go-enry/v2"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/filemode"
)

// maxBlobSize bounds inline text content; larger blobs are marked too large.
const maxBlobSize = 1 << 20 // 1MiB

// the content can be one of the following:
//
// - code      : text |          | raw
// - markup    : text | rendered | raw
// - svg       : text | rendered | raw
// - image     :      | rendered | raw
// - video     :      | rendered | raw
// - submodule :      | rendered |
// - rest      :      |          | raw
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

	l = l.With("ref", ref, "path", filePath)

	ctx := r.Context()

	xrpcc := &indigoxrpc.Client{Host: rp.config.KnotMirror.Url}
	resp, err := tangled.GitTempGetEntry(ctx, xrpcc, filePath, ref, f.RepoDid)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC git.getEntry", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Error503(w)
		return
	}

	var breadcrumbs [][]string
	breadcrumbs = append(breadcrumbs, []string{f.Name, fmt.Sprintf("/%s/tree/%s", reporesolver.GetBaseRepoPath(r, f), url.PathEscape(ref))})
	if filePath != "" {
		for idx, elem := range strings.Split(filePath, "/") {
			breadcrumbs = append(breadcrumbs, []string{elem, fmt.Sprintf("%s/%s", breadcrumbs[idx][1], url.PathEscape(elem))})
		}
	}

	blobView, err := func() (models.BlobView, error) {
		mode, err := filemode.New(resp.Mode)
		if err != nil {
			mode = filemode.Regular
		}

		if mode == filemode.Submodule {
			if resp.Submodule == nil {
				return models.BlobView{}, fmt.Errorf("submodule info is missing")
			}
			return models.BlobView{
				ContentType: models.BlobContentTypeSubmodule,
				ContentSrc:  resp.Submodule.Url,
			}, nil
		}

		blobUrl := generateBlobURL(rp.config.KnotMirror.Url, f, ref, filePath)
		blobReq, err := http.NewRequestWithContext(ctx, http.MethodGet, blobUrl, nil)
		if err != nil {
			return models.BlobView{}, err
		}
		blobResp, err := util.RobustHTTPClient().Do(blobReq)
		if err != nil {
			return models.BlobView{}, err
		}
		defer blobResp.Body.Close()

		if blobResp.StatusCode != http.StatusOK {
			return models.BlobView{}, fmt.Errorf("blob fetch failed: status %d", blobResp.StatusCode)
		}

		// inspect content-type header
		// - text/plain -> Code / Markup
		// - image/svg  -> Svg
		// - image/*    -> Image
		// - video/*    -> Video
		// - */*        -> Other
		mediaType, _, _ := mime.ParseMediaType(blobResp.Header.Get("Content-Type"))
		var contentType models.BlobContentType
		switch {
		case mediaType == "image/svg+xml":
			contentType = models.BlobContentTypeSvg
		case strings.HasPrefix(mediaType, "text/"):
			if markup.GetFormat(filePath) == markup.FormatMarkdown {
				contentType = models.BlobContentTypeMarkup
			} else {
				contentType = models.BlobContentTypeCode
			}
		case strings.HasPrefix(mediaType, "image/"):
			contentType = models.BlobContentTypeImage
		case strings.HasPrefix(mediaType, "video/"):
			contentType = models.BlobContentTypeVideo
		default:
			contentType = models.BlobContentTypeOther
		}

		// only text-viewable content is read inline; others stream via ContentSrc
		if !contentType.HasTextView() {
			return models.BlobView{
				ContentType:  contentType,
				ContentSrc:   blobUrl,
				FileTooLarge: false,
				Contents:     "",
				Lines:        0,
				SizeHint:     uint64(resp.Size),
			}, nil
		}

		// skip large blobs
		if resp.Size > maxBlobSize {
			return models.BlobView{
				ContentType:  contentType,
				ContentSrc:   blobUrl,
				FileTooLarge: true,
				SizeHint:     uint64(resp.Size),
			}, nil
		}

		// just in case, ensure the size again
		content, err := io.ReadAll(io.LimitReader(blobResp.Body, maxBlobSize))
		if err != nil {
			return models.BlobView{}, err
		}

		contentStr := string(content)
		return models.BlobView{
			ContentType:  contentType,
			ContentSrc:   blobUrl,
			Contents:     contentStr,
			FileTooLarge: false,
			Lines:        countLines(contentStr),
			SizeHint:     uint64(resp.Size),
		}, nil
	}()
	if err != nil {
		l.Error("failed to render blob", "err", err)
		rp.pages.Error503(w)
		return
	}

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
		when, _ := time.Parse(time.RFC3339, resp.LastCommit.Committer.When)
		lastCommitInfo = &types.LastCommitInfo{
			Hash:    plumbing.NewHash(derefString(resp.LastCommit.Hash)),
			Message: resp.LastCommit.Message,
			When:    when,
		}
		if resp.LastCommit.Author != nil {
			lastCommitInfo.Author.Name = resp.LastCommit.Author.Name
			lastCommitInfo.Author.Email = resp.LastCommit.Author.Email
			lastCommitInfo.Author.When, _ = time.Parse(time.RFC3339, resp.LastCommit.Author.When)
		}
	}

	baseName := filepath.Base(filePath)
	lang, ok := enry.GetLanguageByExtension(baseName)
	if !ok {
		lang, ok = enry.GetLanguageByFilename(baseName)
	}
	if !ok && blobView.Contents != "" {
		lang = enry.GetLanguage(baseName, []byte(blobView.Contents))
	}
	if group := enry.GetLanguageGroup(lang); group != "" {
		lang = group
	}

	user := rp.oauth.GetMultiAccountUser(r)
	rp.pages.RepoBlob(w, pages.RepoBlobParams{
		BaseParams:     pages.BaseParamsFromContext(r.Context()),
		RepoInfo:       rp.repoResolver.GetRepoInfo(r, user),
		BreadCrumbs:    breadcrumbs,
		BlobView:       blobView,
		EmailToDid:     emailToDidMap,
		LastCommitInfo: lastCommitInfo,
		ShowRendered:   r.URL.Query().Get("code") != "true",
		Ref:            ref,
		Path:           filePath,
		Language:       lang,
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

	blobURL := generateBlobURL(rp.config.KnotMirror.Url, f, ref, filePath)

	if r.URL.Query().Get("download") == "1" {
		req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, blobURL, nil)
		if err != nil {
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		resp, err := util.RobustHTTPClient().Do(req)
		if err != nil || resp.StatusCode != http.StatusOK {
			w.WriteHeader(http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()

		filename := filepath.Base(filePath)
		w.Header().Set("Content-Disposition", fmt.Sprintf(`attachment; filename=%q`, filename))
		w.Header().Set("Content-Type", resp.Header.Get("Content-Type"))
		w.Header().Set("Cache-Control", "public, no-cache")
		io.Copy(w, resp.Body)
		return
	}

	w.Header().Set("Cache-Control", "public, no-cache")
	http.Redirect(w, r, blobURL, http.StatusFound)
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
		view.ContentSrc = resp.Submodule.Url
		return view
	}

	// Determine if binary
	if (resp.IsBinary != nil && *resp.IsBinary) || (resp.FileTooLarge != nil && *resp.FileTooLarge) {
		view.ContentSrc = generateBlobURL(config.KnotMirror.Url, repo, ref, filePath)
		ext := strings.ToLower(filepath.Ext(resp.Path))

		switch ext {
		case ".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".jxl", ".heic", ".heif":
			view.ContentType = models.BlobContentTypeImage

		case ".svg":
			view.ContentType = models.BlobContentTypeSvg
			if resp.Content != nil {
				bytes, _ := base64.StdEncoding.DecodeString(*resp.Content)
				view.Contents = string(bytes)
				view.Lines = countLines(view.Contents)
			}

		case ".mp4", ".webm", ".ogg", ".mov", ".avi":
			view.ContentType = models.BlobContentTypeVideo
		}

		return view
	}

	// otherwise, we are dealing with text content

	if resp.Content != nil {
		view.Contents = *resp.Content
		view.Lines = countLines(view.Contents)
	}

	// with text, we may be dealing with markdown
	format := markup.GetFormat(resp.Path)
	if format == markup.FormatMarkdown {
		view.ContentType = models.BlobContentTypeMarkup
	}

	return view
}

func generateBlobURL(knotmirror string, repo *models.Repo, ref, filePath string) string {
	query := url.Values{}
	query.Set("repo", repo.RepoDid)
	query.Set("ref", ref)
	query.Set("path", filePath)

	blobURL := fmt.Sprintf("%s/xrpc/%s?%s", knotmirror, tangled.GitTempGetBlobNSID, query.Encode())
	return blobURL
	// return path.Join("/", repo.RepoDid, url.PathEscape(ref), filePath)
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
