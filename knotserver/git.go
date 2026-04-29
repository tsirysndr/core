package knotserver

import (
	"compress/gzip"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	securejoin "github.com/cyphar/filepath-securejoin"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/knotserver/git/service"
)

func (h *Knot) resolveRepoPath(r *http.Request) (string, string, error) {
	did := chi.URLParam(r, "did")
	name := chi.URLParam(r, "name")

	if name == "" && strings.HasPrefix(did, "did:") {
		repoPath, _, repoName, err := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, did)
		if err != nil {
			return "", "", fmt.Errorf("unknown repo DID: %w", err)
		}
		return repoPath, repoName, nil
	}

	alias, err := h.db.ResolveAlias(did, name)
	if err == nil && alias != nil {
		repoPath, _, _, resolveErr := h.db.ResolveRepoDIDOnDisk(h.c.Repo.ScanPath, alias.RepoDid)
		if resolveErr == nil {
			return repoPath, name, nil
		}
	}

	repoPath, joinErr := securejoin.SecureJoin(h.c.Repo.ScanPath, filepath.Join(did, name))
	if joinErr != nil {
		return "", "", fmt.Errorf("repo not found: %w", joinErr)
	}
	if _, statErr := os.Stat(repoPath); statErr != nil {
		return "", "", fmt.Errorf("repo not found: %w", statErr)
	}
	return repoPath, name, nil
}

func (h *Knot) repoNotFound(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; charset=UTF-8")
	w.WriteHeader(http.StatusNotFound)
	fmt.Fprint(w, "repository not found\n")
}

func (h *Knot) InfoRefs(w http.ResponseWriter, r *http.Request) {
	repoPath, name, err := h.resolveRepoPath(r)
	if err != nil {
		h.repoNotFound(w, r)
		h.l.Error("git: failed to resolve repo path", "handler", "InfoRefs", "error", err)
		return
	}

	cmd := service.ServiceCommand{
		GitProtocol: r.Header.Get("Git-Protocol"),
		Dir:         repoPath,
		Stdout:      w,
		Sandbox:     h.sandbox,
	}

	serviceName := r.URL.Query().Get("service")
	switch serviceName {
	case "git-upload-pack":
		w.Header().Set("Content-Type", "application/x-git-upload-pack-advertisement")
		w.Header().Set("Connection", "Keep-Alive")
		w.Header().Set("Cache-Control", "no-cache, max-age=0, must-revalidate")
		w.WriteHeader(http.StatusOK)

		if err := cmd.InfoRefs(); err != nil {
			gitError(w, err.Error(), http.StatusInternalServerError)
			h.l.Error("git: process failed", "handler", "InfoRefs", "service", serviceName, "error", err)
			return
		}
	case "git-receive-pack":
		h.RejectPush(w, r, name)
	default:
		gitError(w, fmt.Sprintf("service unsupported: '%s'", serviceName), http.StatusForbidden)
	}
}

func (h *Knot) UploadArchive(w http.ResponseWriter, r *http.Request) {
	repo, _, err := h.resolveRepoPath(r)
	if err != nil {
		h.repoNotFound(w, r)
		h.l.Error("git: failed to resolve repo path", "handler", "UploadArchive", "error", err)
		return
	}

	const expectedContentType = "application/x-git-upload-archive-request"
	contentType := r.Header.Get("Content-Type")
	if contentType != expectedContentType {
		gitError(w, fmt.Sprintf("Expected Content-Type: '%s', but received '%s'.", expectedContentType, contentType), http.StatusUnsupportedMediaType)
	}

	var bodyReader io.ReadCloser = r.Body
	if r.Header.Get("Content-Encoding") == "gzip" {
		gzipReader, err := gzip.NewReader(r.Body)
		if err != nil {
			gitError(w, err.Error(), http.StatusInternalServerError)
			h.l.Error("git: failed to create gzip reader", "handler", "UploadArchive", "error", err)
			return
		}
		defer gzipReader.Close()
		bodyReader = gzipReader
	}

	w.Header().Set("Content-Type", "application/x-git-upload-archive-result")

	h.l.Info("git: executing git-upload-archive", "handler", "UploadArchive", "repo", repo)

	cmd := service.ServiceCommand{
		GitProtocol: r.Header.Get("Git-Protocol"),
		Dir:         repo,
		Stdout:      w,
		Stdin:       bodyReader,
		Sandbox:     h.sandbox,
	}

	w.WriteHeader(http.StatusOK)

	if err := cmd.UploadArchive(); err != nil {
		h.l.Error("git: failed to execute git-upload-pack", "handler", "UploadPack", "error", err)
		return
	}
}

func (h *Knot) UploadPack(w http.ResponseWriter, r *http.Request) {
	repo, _, err := h.resolveRepoPath(r)
	if err != nil {
		h.repoNotFound(w, r)
		h.l.Error("git: failed to resolve repo path", "handler", "UploadPack", "error", err)
		return
	}

	const expectedContentType = "application/x-git-upload-pack-request"
	contentType := r.Header.Get("Content-Type")
	if contentType != expectedContentType {
		gitError(w, fmt.Sprintf("Expected Content-Type: '%s', but received '%s'.", expectedContentType, contentType), http.StatusUnsupportedMediaType)
	}

	var bodyReader io.ReadCloser = r.Body
	if r.Header.Get("Content-Encoding") == "gzip" {
		gzipReader, err := gzip.NewReader(r.Body)
		if err != nil {
			gitError(w, err.Error(), http.StatusInternalServerError)
			h.l.Error("git: failed to create gzip reader", "handler", "UploadPack", "error", err)
			return
		}
		defer gzipReader.Close()
		bodyReader = gzipReader
	}

	w.Header().Set("Content-Type", "application/x-git-upload-pack-result")
	w.Header().Set("Connection", "Keep-Alive")
	w.Header().Set("Cache-Control", "no-cache, max-age=0, must-revalidate")

	h.l.Info("git: executing git-upload-pack", "handler", "UploadPack", "repo", repo)

	cmd := service.ServiceCommand{
		GitProtocol: r.Header.Get("Git-Protocol"),
		Dir:         repo,
		Stdout:      w,
		Stdin:       bodyReader,
		Sandbox:     h.sandbox,
	}

	w.WriteHeader(http.StatusOK)

	if err := cmd.UploadPack(); err != nil {
		h.l.Error("git: failed to execute git-upload-pack", "handler", "UploadPack", "error", err)
		return
	}
}

func (h *Knot) ReceivePack(w http.ResponseWriter, r *http.Request) {
	_, name, err := h.resolveRepoPath(r)
	if err != nil {
		h.repoNotFound(w, r)
		h.l.Error("git: failed to resolve repo path", "handler", "ReceivePack", "error", err)
		return
	}

	h.RejectPush(w, r, name)
}

func (h *Knot) RejectPush(w http.ResponseWriter, r *http.Request, unqualifiedRepoName string) {
	// A text/plain response will cause git to print each line of the body
	// prefixed with "remote: ".
	w.Header().Set("content-type", "text/plain; charset=UTF-8")
	w.WriteHeader(http.StatusForbidden)

	fmt.Fprintf(w, "Pushes are only supported over SSH.")

	// If the appview gave us the repository owner's handle we can attempt to
	// construct the correct ssh url.
	ownerHandle := r.Header.Get("x-tangled-repo-owner-handle")
	ownerHandle = strings.TrimPrefix(ownerHandle, "@")
	if ownerHandle != "" && !strings.ContainsAny(ownerHandle, ":") {
		hostname := h.c.Server.Hostname
		if strings.Contains(hostname, ":") {
			hostname = strings.Split(hostname, ":")[0]
		}

		if hostname == "knot1.tangled.sh" {
			hostname = "tangled.sh"
		}

		fmt.Fprintf(w, " Try:\ngit remote set-url --push origin git@%s:%s/%s\n\n... and push again.", hostname, ownerHandle, unqualifiedRepoName)
	}
	fmt.Fprintf(w, "\n\n")
}

func isDir(path string) (bool, error) {
	info, err := os.Stat(path)
	if err == nil && info.IsDir() {
		return true, nil
	}
	if os.IsNotExist(err) {
		return false, nil
	}
	return false, err
}

func gitError(w http.ResponseWriter, msg string, status int) {
	w.Header().Set("content-type", "text/plain; charset=UTF-8")
	w.WriteHeader(status)
	fmt.Fprintf(w, "%s\n", msg)
}
