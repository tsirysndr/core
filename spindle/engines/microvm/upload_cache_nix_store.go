//go:build linux

package microvm

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// we have an interface for running commands so we can swap it in tests
type CommandRunner interface {
	Run(ctx context.Context, name string, args ...string) error
}

type execRunner struct{}

func (execRunner) Run(ctx context.Context, name string, args ...string) error {
	// nosemgrep: go.lang.security.audit.dangerous-exec-command.dangerous-exec-command
	cmd := exec.CommandContext(ctx, name, args...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%s %s: %w\n%s", name, strings.Join(args, " "), err, string(out))
	}
	return nil
}

const (
	nixStoreCacheInfo = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 50\n"
	maxNarUploadSize  = 5 << 30 // 5gib
)

type NixStoreUploadBackend struct {
	stagingDir       string
	targetStore      string
	readUpstreams    []CacheUpstream
	logger           *slog.Logger
	runner           CommandRunner
	maxNarUploadSize int64
}

func newNixStoreUploadBackend(targetStore, stagingDir string, readUpstreams []CacheUpstream, logger *slog.Logger, runner CommandRunner) (*NixStoreUploadBackend, error) {
	absStaging, err := filepath.Abs(stagingDir)
	if err != nil {
		return nil, fmt.Errorf("resolve staging dir %q: %w", stagingDir, err)
	}
	if logger == nil {
		logger = slog.Default()
	}

	if err := os.MkdirAll(filepath.Join(absStaging, "nar"), 0o755); err != nil {
		return nil, fmt.Errorf("create staging cache directories: %w", err)
	}
	infoPath := filepath.Join(absStaging, "nix-cache-info")
	if _, err := os.Stat(infoPath); errors.Is(err, os.ErrNotExist) {
		if err := os.WriteFile(infoPath, []byte(nixStoreCacheInfo), 0o644); err != nil {
			return nil, fmt.Errorf("write nix-cache-info: %w", err)
		}
	}

	if runner == nil {
		runner = execRunner{}
	}

	return &NixStoreUploadBackend{
		stagingDir:       absStaging,
		targetStore:      targetStore,
		readUpstreams:    readUpstreams,
		logger:           logger,
		runner:           runner,
		maxNarUploadSize: maxNarUploadSize,
	}, nil
}

func (b *NixStoreUploadBackend) Close() error { return nil }

func (b *NixStoreUploadBackend) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	relPath, err := normalizeUploadCachePath(r.URL.Path)
	if err != nil {
		b.logger.Warn("refusing upload cache request with unsafe path", "path", r.URL.Path, "error", err)
		http.Error(w, "invalid path", http.StatusBadRequest)
		return
	}

	switch r.Method {
	case http.MethodGet, http.MethodHead:
		switch {
		case relPath == "nix-cache-info":
			b.serveCacheInfo(w, r)
			return
		case isNarinfoObjectPath(relPath):
			b.serveNarinfo(w, r, relPath)
			return
		}

	case http.MethodPut:
		switch {
		case relPath == "nix-cache-info":
			b.putCacheInfo(w, r)
			return
		case isNarObjectPath(relPath):
			b.putNar(w, r, relPath)
			return
		case isNarinfoObjectPath(relPath):
			b.putNarinfo(w, r, relPath)
			return
		}
	}

	http.Error(w, "not found", http.StatusNotFound)
}

func (b *NixStoreUploadBackend) serveCacheInfo(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/x-nix-cache-info")
	w.Header().Set("Content-Length", fmt.Sprintf("%d", len(nixStoreCacheInfo)))
	if r.Method == http.MethodHead {
		w.WriteHeader(http.StatusOK)
		return
	}
	_, _ = w.Write([]byte(nixStoreCacheInfo))
}

func (b *NixStoreUploadBackend) putCacheInfo(w http.ResponseWriter, r *http.Request) {
	_, _ = io.Copy(io.Discard, io.LimitReader(r.Body, int64(len(nixStoreCacheInfo))+1))
	w.WriteHeader(http.StatusOK)
}

func (b *NixStoreUploadBackend) serveNarinfo(w http.ResponseWriter, r *http.Request, relPath string) {
	localPath, err := b.stagingObjectPath(relPath)
	if err != nil {
		b.logger.Warn("refusing narinfo request with unsafe path", "path", relPath, "error", err)
		http.Error(w, "invalid path", http.StatusBadRequest)
		return
	}

	fi, err := os.Stat(localPath)
	if err == nil && !fi.IsDir() {
		if _, err := readNarinfoFile(localPath); err != nil {
			b.logger.Warn("staged narinfo is invalid", "path", relPath, "error", err)
			http.Error(w, "invalid staged narinfo", http.StatusInternalServerError)
			return
		}
		b.serveLocalFile(w, r, localPath, fi)
		return
	}
	if !errors.Is(err, os.ErrNotExist) {
		b.logger.Warn("stat staged narinfo failed", "path", relPath, "error", err)
	}

	if len(b.readUpstreams) > 0 {
		probe := r.Clone(r.Context())
		probe.URL.Path = "/" + relPath
		serveNarinfoExistence(w, probe, newNarinfoExistenceTransport(b.readUpstreams, b.logger), b.logger)
		return
	}

	http.Error(w, "not found", http.StatusNotFound)
}

func (b *NixStoreUploadBackend) serveLocalFile(w http.ResponseWriter, r *http.Request, localPath string, fi os.FileInfo) {
	w.Header().Set("Content-Type", "text/x-nix-narinfo")
	w.Header().Set("Content-Length", fmt.Sprintf("%d", fi.Size()))
	w.Header().Set("Last-Modified", fi.ModTime().UTC().Format(http.TimeFormat))

	if r.Method == http.MethodHead {
		w.WriteHeader(http.StatusOK)
		return
	}

	f, err := os.Open(localPath)
	if err != nil {
		b.logger.Warn("open staged narinfo failed", "path", localPath, "error", err)
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	defer f.Close()
	w.WriteHeader(http.StatusOK)
	if _, err := io.Copy(w, f); err != nil && !errors.Is(err, context.Canceled) {
		b.logger.Warn("copy staged narinfo failed", "path", localPath, "error", err)
	}
}

func (b *NixStoreUploadBackend) putNar(w http.ResponseWriter, r *http.Request, relPath string) {
	name := strings.TrimPrefix(relPath, "nar/")
	dst, err := b.stagingObjectPath(relPath)
	if err != nil {
		b.logger.Warn("refusing nar upload with unsafe path", "name", name, "error", err)
		http.Error(w, "invalid nar path", http.StatusBadRequest)
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, b.maxNarUploadSize)

	var copyErr error
	written, err := writeFileAtomic(dst, ".tmp-nar", func(f *os.File) (int64, error) {
		n, err := io.Copy(f, r.Body)
		copyErr = err
		return n, err
	})
	if err != nil {
		b.logger.Warn("stage nar upload failed", "name", name, "error", err)
		var maxErr *http.MaxBytesError
		if errors.As(err, &maxErr) {
			http.Error(w, "nar too large", http.StatusRequestEntityTooLarge)
			return
		}
		if copyErr != nil {
			http.Error(w, "upload failed", http.StatusBadRequest)
			return
		}
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}

	b.logger.Debug("staged nar", "name", name, "bytes", written)
	w.WriteHeader(http.StatusOK)
}

func (b *NixStoreUploadBackend) putNarinfo(w http.ResponseWriter, r *http.Request, relPath string) {
	body, err := io.ReadAll(io.LimitReader(r.Body, maxNarinfoSize+1))
	if err != nil {
		b.logger.Warn("read narinfo body failed", "path", relPath, "error", err)
		http.Error(w, "upload failed", http.StatusBadRequest)
		return
	}
	if len(body) > maxNarinfoSize {
		b.logger.Warn("narinfo body exceeds maximum size", "path", relPath, "bytes", len(body))
		http.Error(w, "narinfo too large", http.StatusBadRequest)
		return
	}

	info, err := parseNarinfo(bytes.NewReader(body))
	if err != nil {
		b.logger.Warn("refusing narinfo upload with invalid body", "path", relPath, "error", err)
		http.Error(w, "invalid narinfo: "+err.Error(), http.StatusBadRequest)
		return
	}
	storePathHash, _, err := parseStorePath(info.StorePath)
	if err != nil {
		b.logger.Warn("refusing narinfo upload with invalid store path", "path", relPath, "storePath", info.StorePath, "error", err)
		http.Error(w, "invalid StorePath", http.StatusBadRequest)
		return
	}
	fileHash := strings.TrimSuffix(filepath.Base(relPath), ".narinfo")
	if fileHash != storePathHash {
		b.logger.Warn("refusing narinfo upload with mismatched filename hash", "path", relPath, "storePath", info.StorePath)
		http.Error(w, "narinfo filename does not match StorePath hash", http.StatusBadRequest)
		return
	}
	if !isNarObjectPath(info.URL) {
		b.logger.Warn("narinfo references invalid nar URL", "path", relPath, "url", info.URL)
		http.Error(w, "invalid nar URL", http.StatusBadRequest)
		return
	}

	narPath, err := b.stagingObjectPath(info.URL)
	if err != nil {
		b.logger.Warn("narinfo references unsafe nar URL", "path", relPath, "url", info.URL, "error", err)
		http.Error(w, "invalid nar URL", http.StatusBadRequest)
		return
	}
	if _, err := os.Stat(narPath); err != nil {
		b.logger.Warn("narinfo references missing nar", "path", relPath, "url", info.URL, "error", err)
		http.Error(w, "referenced nar does not exist", http.StatusBadRequest)
		return
	}

	dst, err := b.stagingObjectPath(relPath)
	if err != nil {
		b.logger.Warn("refusing narinfo upload with unsafe path", "path", relPath, "error", err)
		http.Error(w, "invalid path", http.StatusBadRequest)
		return
	}

	if _, err := writeNarinfoFile(dst, body); err != nil {
		b.logger.Warn("stage narinfo upload failed", "path", relPath, "error", err)
		http.Error(w, "internal error", http.StatusInternalServerError)
		return
	}

	if err := b.importStorePath(r.Context(), info.StorePath); err != nil {
		b.logger.Warn("import staged narinfo failed", "path", relPath, "storePath", info.StorePath, "error", err)
		if cleanupErr := removeFileAndSyncDir(dst); cleanupErr != nil {
			b.logger.Error("remove staged narinfo after failed import", "path", relPath, "error", cleanupErr)
		}
		http.Error(w, "import failed", http.StatusBadGateway)
		return
	}

	b.logger.Debug("staged narinfo", "path", relPath, "storePath", info.StorePath)
	w.WriteHeader(http.StatusOK)
}

func normalizeUploadCachePath(path string) (string, error) {
	if path == "" || path == "/" {
		return "", fmt.Errorf("empty path")
	}
	if !strings.HasPrefix(path, "/") {
		return "", fmt.Errorf("path must start with /")
	}
	if strings.Contains(path, "..") {
		return "", fmt.Errorf("path traversal")
	}

	return strings.TrimPrefix(path, "/"), nil
}

func isNarinfoObjectPath(relPath string) bool {
	if !strings.HasSuffix(relPath, ".narinfo") {
		return false
	}
	if relPath != filepath.Base(relPath) {
		return false
	}
	base := filepath.Base(relPath)
	return base != "" && base != "." && base != ".narinfo"
}

func isNarObjectPath(relPath string) bool {
	if !strings.HasPrefix(relPath, "nar/") {
		return false
	}
	name := strings.TrimPrefix(relPath, "nar/")
	return name != "" && name != "." && name == filepath.Base(name) && !strings.Contains(name, "/")
}

func (b *NixStoreUploadBackend) stagingObjectPath(relPath string) (string, error) {
	if !isNarObjectPath(relPath) && !isNarinfoObjectPath(relPath) {
		return "", fmt.Errorf("invalid cache object path %q", relPath)
	}

	local, err := filepath.Localize(relPath)
	if err != nil {
		return "", fmt.Errorf("unsafe cache object path %q: %w", relPath, err)
	}

	return filepath.Join(b.stagingDir, local), nil
}

// makes the full reference graph of rootStorePath resolvable in the staging
// cache. `nix copy` computes the closure from the --from store, so every
// referenced narinfo must be present there or the walk fails with "path ... is
// not valid". newly-built deps are already staged by the guest, but deps that
// live only in a read cache were skipped during upload, so we backfill their
// narinfos here. only the narinfos (the reference graph) are needed: the
// destination supplies the NAR data via --substitute-on-destination.
func (b *NixStoreUploadBackend) ensureClosureStaged(ctx context.Context, rootStorePath string) error {
	visited := map[string]bool{}
	queue := []string{rootStorePath}
	for len(queue) > 0 {
		storePath := queue[0]
		queue = queue[1:]
		if visited[storePath] {
			continue
		}
		visited[storePath] = true

		info, err := b.resolveStagedNarinfo(ctx, storePath)
		if err != nil {
			// the root must resolve (the guest just staged it); a dep we can't
			// find anywhere is left for `nix copy` to surface with its own error.
			if storePath == rootStorePath {
				return fmt.Errorf("resolve narinfo for %s: %w", storePath, err)
			}
			b.logger.Warn("closure dep narinfo unresolved; leaving to nix copy", "storePath", storePath, "error", err)
			continue
		}

		for _, ref := range info.References {
			refPath := storePrefix + ref
			if refPath == storePath {
				continue // self-reference
			}
			if !visited[refPath] {
				queue = append(queue, refPath)
			}
		}
	}
	return nil
}

// returns parsed narinfo for store path, backfilling from readUpstreams if not found
func (b *NixStoreUploadBackend) resolveStagedNarinfo(ctx context.Context, storePath string) (*narinfo, error) {
	hash, _, err := parseStorePath(storePath)
	if err != nil {
		return nil, err
	}
	localPath := filepath.Join(b.stagingDir, hash+".narinfo")
	info, err := readNarinfoFile(localPath)
	if err == nil {
		return info, nil
	}
	if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}

	// dep missing from staging because it was skipped during upload
	// (lives on a read cache) so we backfill it from readUpstreams.
	body, err := b.fetchUpstreamNarinfo(ctx, hash)
	if err != nil {
		return nil, err
	}
	written, err := writeNarinfoFile(localPath, body)
	if err != nil {
		return nil, err
	}
	b.logger.Debug("backfilled narinfo", "hash", hash, "bytes", written)

	return parseNarinfo(bytes.NewReader(body))
}

// fetches narinfo from readUpstreams
func (b *NixStoreUploadBackend) fetchUpstreamNarinfo(ctx context.Context, hash string) ([]byte, error) {
	if len(b.readUpstreams) == 0 {
		return nil, os.ErrNotExist
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, "http://upstream/"+hash+".narinfo", nil)
	if err != nil {
		return nil, err
	}
	resp, err := newNarinfoExistenceTransport(b.readUpstreams, b.logger).RoundTrip(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound {
		return nil, os.ErrNotExist
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("upstream narinfo %s: status %d", hash, resp.StatusCode)
	}
	return io.ReadAll(io.LimitReader(resp.Body, maxNarinfoSize+1))
}

// todo(dawn): ideally we don't use `nix copy` here but instead have our own
// `nix copy` impl so we don't need nix on host. but that's a far stretch goal :p
func (b *NixStoreUploadBackend) importStorePath(ctx context.Context, storePath string) error {
	if err := b.ensureClosureStaged(ctx, storePath); err != nil {
		return fmt.Errorf("stage closure for %s: %w", storePath, err)
	}

	fromURL := url.URL{Scheme: "file", Path: b.stagingDir}
	args := []string{
		"copy",
		"--from", fromURL.String(),
		"--to", b.targetStore,
		// todo(dawn): ideally we support signing in spindle itself.
		// but for now harmonia can sign things on serve so this is ok.
		"--no-check-sigs",
		"--substitute-on-destination",
		storePath,
	}

	b.logger.Info("importing staged cache path", "target", b.targetStore, "storePath", storePath)
	if err := b.runner.Run(ctx, "nix", args...); err != nil {
		return fmt.Errorf("nix copy to %s: %w", b.targetStore, err)
	}
	return nil
}

func readNarinfoFile(path string) (*narinfo, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	return parseNarinfo(f)
}

func writeNarinfoFile(path string, body []byte) (int64, error) {
	return writeFileAtomic(path, ".tmp-narinfo", func(f *os.File) (int64, error) {
		n, err := f.Write(body)
		return int64(n), err
	})
}

func writeFileAtomic(dst, tempPrefix string, write func(*os.File) (int64, error)) (written int64, err error) {
	dir := filepath.Dir(dst)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return 0, fmt.Errorf("create directory %q: %w", dir, err)
	}

	tmp, err := os.CreateTemp(dir, tempPrefix+"-*-"+filepath.Base(dst))
	if err != nil {
		return 0, fmt.Errorf("create temporary file in %q: %w", dir, err)
	}
	tmpName := tmp.Name()
	defer func() {
		if err != nil {
			_ = tmp.Close()
			_ = os.Remove(tmpName)
		}
	}()

	written, err = write(tmp)
	if err != nil {
		return 0, err
	}
	if err := tmp.Sync(); err != nil {
		return 0, fmt.Errorf("fsync temporary file %q: %w", tmpName, err)
	}
	if err := tmp.Close(); err != nil {
		return 0, fmt.Errorf("close temporary file %q: %w", tmpName, err)
	}
	if err := os.Rename(tmpName, dst); err != nil {
		return 0, fmt.Errorf("rename %q to %q: %w", tmpName, dst, err)
	}

	if err := syncDir(dir); err != nil {
		return 0, err
	}

	return written, nil
}

func removeFileAndSyncDir(path string) error {
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("remove %q: %w", path, err)
	}
	return syncDir(filepath.Dir(path))
}

func syncDir(dir string) error {
	dirFile, err := os.Open(dir)
	if err != nil {
		return fmt.Errorf("open directory %q: %w", dir, err)
	}
	defer dirFile.Close()
	if err := dirFile.Sync(); err != nil {
		return fmt.Errorf("sync directory %q: %w", dir, err)
	}
	return nil
}
