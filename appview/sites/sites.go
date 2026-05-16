package sites

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/models"
)

// DomainMapping is the value stored in Workers KV, keyed by the bare domain.
// Repos maps repo name → RepoEntry; at most one repo may have IsIndex = true.
type DomainMapping struct {
	Did   string               `json:"did"`
	Repos map[string]RepoEntry `json:"repos"`
}

// RepoEntry is the per-repo value within a DomainMapping. Rkey is the
// repository's atproto record key, which identifies the {did}/{rkey}/
// prefix in R2 where the site's objects live.
type RepoEntry struct {
	Rkey    string `json:"rkey"`
	IsIndex bool   `json:"is_index"`
}

// UnmarshalJSON makes DomainMapping tolerant of the legacy KV shape where
// repos was map[string]bool (keyed by rkey, value = is_index). For each
// entry it tries the new {rkey, is_index} struct first; if that fails it
// falls back to a bare bool, using the map key itself as the rkey.
func (m *DomainMapping) UnmarshalJSON(data []byte) error {
	var raw struct {
		Did   string                     `json:"did"`
		Repos map[string]json.RawMessage `json:"repos"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	m.Did = raw.Did
	m.Repos = make(map[string]RepoEntry, len(raw.Repos))
	for name, val := range raw.Repos {
		var entry RepoEntry
		if err := json.Unmarshal(val, &entry); err == nil {
			m.Repos[name] = entry
			continue
		}
		// legacy shape: value is a bare bool; map key is the rkey
		var isIndex bool
		if err := json.Unmarshal(val, &isIndex); err != nil {
			return fmt.Errorf("unsupported repo entry for %q: %w", name, err)
		}
		m.Repos[name] = RepoEntry{Rkey: name, IsIndex: isIndex}
	}
	return nil
}

// getOrNewMapping fetches the existing KV entry for domain, or returns a
// fresh empty mapping for the given did if none exists yet.
func getOrNewMapping(ctx context.Context, cf *cloudflare.Client, domain, did string) (DomainMapping, error) {
	raw, err := cf.KVGet(ctx, domain)
	if err != nil {
		return DomainMapping{}, fmt.Errorf("reading domain mapping for %q: %w", domain, err)
	}
	if raw == nil {
		return DomainMapping{Did: did, Repos: make(map[string]RepoEntry)}, nil
	}
	var m DomainMapping
	if err := json.Unmarshal(raw, &m); err != nil {
		return DomainMapping{}, fmt.Errorf("unmarshalling domain mapping for %q: %w", domain, err)
	}
	if m.Repos == nil {
		m.Repos = make(map[string]RepoEntry)
	}
	return m, nil
}

// PutDomainMapping adds or updates a single repo entry within the per-domain
// KV record. If isIndex is true, any previously indexed repo is demoted first.
func PutDomainMapping(ctx context.Context, cf *cloudflare.Client, domain, did, repoName, repoRkey string, isIndex bool) error {
	m, err := getOrNewMapping(ctx, cf, domain, did)
	if err != nil {
		return err
	}

	m.Did = did

	if isIndex {
		for name, entry := range m.Repos {
			if name == repoName {
				continue
			}
			if entry.IsIndex {
				entry.IsIndex = false
				m.Repos[name] = entry
			}
		}
	}

	m.Repos[repoName] = RepoEntry{Rkey: repoRkey, IsIndex: isIndex}

	val, err := json.Marshal(m)
	if err != nil {
		return fmt.Errorf("marshalling domain mapping: %w", err)
	}
	if err := cf.KVPut(ctx, domain, val); err != nil {
		return fmt.Errorf("putting domain mapping for %q: %w", domain, err)
	}
	return nil
}

// DeleteDomainMapping removes a single repo from the per-domain KV record.
// If it was the last repo, the key is deleted entirely.
func DeleteDomainMapping(ctx context.Context, cf *cloudflare.Client, domain, repoName string) error {
	m, err := getOrNewMapping(ctx, cf, domain, "")
	if err != nil {
		return err
	}

	delete(m.Repos, repoName)

	if len(m.Repos) == 0 {
		if err := cf.KVDelete(ctx, domain); err != nil {
			return fmt.Errorf("deleting domain mapping for %q: %w", domain, err)
		}
		return nil
	}

	val, err := json.Marshal(m)
	if err != nil {
		return fmt.Errorf("marshalling domain mapping: %w", err)
	}
	if err := cf.KVPut(ctx, domain, val); err != nil {
		return fmt.Errorf("putting domain mapping for %q: %w", domain, err)
	}
	return nil
}

// DeleteAllDomainMappings removes the KV entry for a domain entirely.
// Used when a user releases their domain claim.
func DeleteAllDomainMappings(ctx context.Context, cf *cloudflare.Client, domain string) error {
	if err := cf.KVDelete(ctx, domain); err != nil {
		return fmt.Errorf("deleting all domain mappings for %q: %w", domain, err)
	}
	return nil
}

// prefix returns the R2 key prefix for a given repo: "{did}/{repo}/".
// All site objects live under this prefix.
func prefix(repoDid, repoName string) string {
	return repoDid + "/" + repoName + "/"
}

// Deploy fetches the repo archive at the given branch from knotHost, extracts
// deployDir from it, and syncs the resulting files to R2 via cf.SyncFiles.
// It is the authoritative entry-point for deploying a git site.
func Deploy(
	ctx context.Context,
	cf *cloudflare.Client,
	config *config.Config,
	f *models.Repo,
	branch string,
	deployDir string,
) error {
	tmpDir, err := os.MkdirTemp("", "tangled-sites-*")
	if err != nil {
		return fmt.Errorf("creating temp dir: %w", err)
	}
	defer os.RemoveAll(tmpDir)

	if err := extractArchive(ctx, config, f, branch, tmpDir); err != nil {
		return fmt.Errorf("extracting archive: %w", err)
	}

	// deployDir is absolute within the repo (e.g. "/" or "/docs").
	// Map it to a path inside tmpDir.
	deployRoot := filepath.Join(tmpDir, filepath.FromSlash(deployDir))

	files := make(map[string][]byte)
	err = filepath.WalkDir(deployRoot, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		content, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(deployRoot, p)
		if err != nil {
			return err
		}
		files[filepath.ToSlash(rel)] = content
		return nil
	})
	if err != nil {
		return fmt.Errorf("walking deploy dir: %w", err)
	}

	if err := cf.SyncFiles(ctx, prefix(f.Did, f.Rkey), files); err != nil {
		return fmt.Errorf("syncing files to R2: %w", err)
	}

	return nil
}

// Delete removes all R2 objects for a repo site.
func Delete(ctx context.Context, cf *cloudflare.Client, repoDid, repoName string) error {
	if err := cf.DeleteFiles(ctx, prefix(repoDid, repoName)); err != nil {
		return fmt.Errorf("deleting site files from R2: %w", err)
	}
	return nil
}

// extractArchive fetches the tar.gz archive for the given repo+branch from
// the knot via XRPC and extracts it into destDir.
func extractArchive(ctx context.Context, config *config.Config, f *models.Repo, branch, destDir string) error {
	scheme := "https"
	if config.Core.Dev {
		scheme = "http"
	}
	knotHost := fmt.Sprintf("%s://%s", scheme, f.Knot)

	xrpcc := &indigoxrpc.Client{Host: knotHost}
	data, err := tangled.RepoArchive(ctx, xrpcc, "tar.gz", "", branch, f.RepoIdentifier())
	if err != nil {
		return fmt.Errorf("fetching archive: %w", err)
	}

	gz, err := gzip.NewReader(bytes.NewReader(data))
	if err != nil {
		return fmt.Errorf("opening gzip stream: %w", err)
	}
	defer gz.Close()

	tr := tar.NewReader(gz)
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("reading tar: %w", err)
		}

		// The knot always adds a leading prefix dir (e.g. "myrepo-main/"); strip it.
		name := hdr.Name
		i := strings.Index(name, "/")
		if i < 0 {
			continue
		}
		name = name[i+1:]
		if name == "" {
			continue
		}

		target := filepath.Join(destDir, filepath.FromSlash(name))

		// Guard against zip-slip.
		if !strings.HasPrefix(target, filepath.Clean(destDir)+string(os.PathSeparator)) {
			continue
		}

		switch hdr.Typeflag {
		case tar.TypeDir:
			if err := os.MkdirAll(target, 0o755); err != nil {
				return err
			}
		case tar.TypeReg:
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			f, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, hdr.FileInfo().Mode())
			if err != nil {
				return err
			}
			if _, err := io.Copy(f, tr); err != nil {
				f.Close()
				return err
			}
			f.Close()
		}
	}

	return nil
}
