package state

import (
	"context"
	"fmt"
	"io"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/models"
)

// allowedResponseHeaders is the set of headers we will forward from the knot
// back to the client. everything else is stripped.
var allowedResponseHeaders = map[string]bool{
	"Content-Encoding":  true,
	"Transfer-Encoding": true,
	"Cache-Control":     true,
	"Expires":           true,
	"Pragma":            true,
}

func copyAllowedHeaders(dst, src http.Header) {
	for k, vv := range src {
		if allowedResponseHeaders[http.CanonicalHeaderKey(k)] {
			for _, v := range vv {
				dst.Add(k, v)
			}
		}
	}
}

func setGitHeaders(w http.ResponseWriter, contentType string) {
	w.Header().Set("Content-Type", contentType)
	w.Header().Set("Content-Disposition", "attachment")
	w.Header().Set("X-Content-Type-Options", "nosniff")
}

func (s *State) InfoRefs(w http.ResponseWriter, r *http.Request) {
	repo := r.Context().Value("repo").(*models.Repo)

	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}

	service := r.URL.Query().Get("service")
	var contentType string
	switch service {
	case "git-receive-pack":
		contentType = "application/x-git-receive-pack-advertisement"
	default:
		contentType = "application/x-git-upload-pack-advertisement"
		go s.notifier.Clone(context.Background(), repo)
	}

	targetURL := fmt.Sprintf("%s://%s/%s/info/refs?%s", scheme, repo.Knot, repo.RepoIdentifier(), r.URL.RawQuery)
	s.proxyRequest(w, r, targetURL, contentType)
}

func (s *State) UploadArchive(w http.ResponseWriter, r *http.Request) {
	repo := r.Context().Value("repo").(*models.Repo)

	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}

	targetURL := fmt.Sprintf("%s://%s/%s/git-upload-archive?%s", scheme, repo.Knot, repo.RepoIdentifier(), r.URL.RawQuery)
	s.proxyRequest(w, r, targetURL, "application/x-git-upload-archive-result")
}

func (s *State) UploadPack(w http.ResponseWriter, r *http.Request) {
	repo := r.Context().Value("repo").(*models.Repo)

	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}

	targetURL := fmt.Sprintf("%s://%s/%s/git-upload-pack?%s", scheme, repo.Knot, repo.RepoIdentifier(), r.URL.RawQuery)
	s.proxyRequest(w, r, targetURL, "application/x-git-upload-pack-result")
}

func (s *State) ReceivePack(w http.ResponseWriter, r *http.Request) {
	repo := r.Context().Value("repo").(*models.Repo)

	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}

	targetURL := fmt.Sprintf("%s://%s/%s/git-receive-pack?%s", scheme, repo.Knot, repo.RepoIdentifier(), r.URL.RawQuery)
	s.proxyRequest(w, r, targetURL, "application/x-git-receive-pack-result")
}

func (s *State) proxyRequest(w http.ResponseWriter, r *http.Request, targetURL string, contentType string) {
	client := &http.Client{}

	proxyReq, err := http.NewRequest(r.Method, targetURL, r.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	proxyReq.Header = r.Header.Clone()

	repoOwnerHandle := chi.URLParam(r, "user")
	if id, ok := r.Context().Value("resolvedId").(identity.Identity); ok && !id.Handle.IsInvalidHandle() {
		repoOwnerHandle = id.Handle.String()
	}
	proxyReq.Header.Set("x-tangled-repo-owner-handle", repoOwnerHandle)

	resp, err := client.Do(proxyReq)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	defer resp.Body.Close()

	// selectively copy only allowed headers
	copyAllowedHeaders(w.Header(), resp.Header)

	setGitHeaders(w, contentType)

	w.WriteHeader(resp.StatusCode)

	if _, err := io.Copy(w, resp.Body); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
}
