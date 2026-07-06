package repoindexer

import (
	"bufio"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"path/filepath"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-enry/go-enry/v2"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/redis/go-redis/v9"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/knotstream"
	"tangled.org/core/knotmirror/xrpc/gitea"
	"tangled.org/core/log"
)

const (
	fileSizeLimit          = 16 * 1024          // read up to 16 KiB for language detection
	bigFileSize            = 1024 * 1024        // skip content read for blobs over 1 MiB
	langIndexRepoCommit    = "lang_index:%s:%s" // lang_index:{did}:{oid}
	langIndexRepoCommitTTL = 30 * 24 * time.Hour
)

// Language indexing strategy:
//
//     git.refUpdate HEAD -> store to db
//     git.refUpdate other -> on-demand calculation, cache
//
// NOTE: currently all event we have is git.refUpdate, so background indexing
//       job will always be triggered.
// TODO(boltless): don't queue indexing job on "sync" type event while repo is
//       not active.

type Indexer struct {
	logger *slog.Logger
	cfg    *config.Config
	rdb    *redis.Client
}

func NewIndexer(l *slog.Logger, cfg *config.Config, rdb *redis.Client) *Indexer {
	indexer := &Indexer{
		logger: log.SubLogger(l, "indexer"),
		cfg:    cfg,
		rdb:    rdb,
	}
	return indexer
}

func NewBackgroundIndexScheduler(l *slog.Logger, cfg *config.Config, e *sql.DB, indexer *Indexer) *knotstream.ParallelScheduler {
	return knotstream.NewParallelScheduler(
		4,
		"repo_stats_update", // NOTE: this is unused
		func(ctx context.Context, t *knotstream.Task) error {
			start := time.Now()
			repoId := syntax.DID(t.Key)

			// resolve HEAD to commitId
			commit, err := gitea.GetCommit(ctx, indexer.repoPath(repoId), "HEAD")
			if err != nil {
				return fmt.Errorf("failed to resolve HEAD: %w", err)
			}

			l := l.With("repo", repoId, "hash", commit.Hash)

			// check if (did,oid) is already indexed
			indexed, err := db.IsLanguageIndexed(ctx, e, repoId, commit.Hash)
			if err != nil {
				l.Error("failed to query langs", "err", err)
				indexed = false
				// continue
			}
			if indexed {
				return nil
			}

			langs, err := indexer.IndexLanguages(ctx, repoId, commit.Hash)
			if err != nil {
				return fmt.Errorf("indexing langs: %w", err)
			}

			l.Info("pre-indexed language stats", "duration", time.Since(start))

			if err := db.InsertLanguages(ctx, e, repoId, commit.Hash, langs); err != nil {
				return fmt.Errorf("failed to insert langs into db: %w", err)
			}

			// HACK(boltless): ping appview to update language cache.
			go func() {
				url := fmt.Sprintf("%s/%s", cfg.AppviewUrl, repoId.String())
				pingCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
				defer cancel()
				req, err := http.NewRequestWithContext(pingCtx, http.MethodGet, url, nil)
				if err != nil {
					l.Warn("appview ping: build request failed", "err", err)
					return
				}
				resp, err := http.DefaultClient.Do(req)
				if err != nil {
					l.Warn("appview ping failed", "url", url, "err", err)
					return
				}
				defer resp.Body.Close()
				// drain body to ensure the appview completes rendering
				if _, err := io.Copy(io.Discard, resp.Body); err != nil {
					l.Warn("appview ping: drain response failed", "url", url, "err", err)
					return
				}
				l.Info("appview pinged", "url", url, "status", resp.StatusCode)
			}()

			return nil
		},
	)
}

func (i *Indexer) repoPath(repo syntax.DID) string {
	return filepath.Join(i.cfg.GitRepoBasePath, repo.String())
}

// IndexLanguages index the repository language stats at given commit
func (i *Indexer) IndexLanguages(ctx context.Context, repoId syntax.DID, commitId plumbing.Hash) (map[string]int64, error) {
	if i.rdb != nil {
		if val, err := i.rdb.Get(ctx, fmt.Sprintf(langIndexRepoCommit, repoId, commitId.String())).Result(); err == nil {
			i.logger.Debug("serve from cache")
			var sizes map[string]int64
			if err := json.Unmarshal([]byte(val), &sizes); err == nil {
				return sizes, nil
			}
		}
	}

	sizes, err := IndexLanguagesInner(ctx, i.repoPath(repoId), commitId)
	if err != nil {
		return nil, err
	}

	if i.rdb != nil {
		if encoded, err := json.Marshal(sizes); err == nil {
			i.logger.Debug("cache language")
			if err := i.rdb.Set(ctx,
				fmt.Sprintf(langIndexRepoCommit, repoId, commitId.String()),
				encoded,
				langIndexRepoCommitTTL,
			).Err(); err != nil {
				i.logger.Error("failed to cache languages", "err", err)
			}
		}
	}
	return sizes, nil
}

func IndexLanguagesInner(ctx context.Context, repoPath string, commitId plumbing.Hash) (map[string]int64, error) {
	tree, err := gitea.GetTree(ctx, repoPath, commitId.String()+"^{tree}")
	if err != nil {
		return nil, err
	}

	bw, br, close := gitea.CatFileBatch(ctx, repoPath)
	defer close()

	sizes, err := batchAnalyzeTree(ctx, bw, br, tree)
	if err != nil {
		return nil, err
	}
	return sizes, nil
}

func batchAnalyzeTree(ctx context.Context, bw io.WriteCloser, br *bufio.Reader, tree *object.Tree) (map[string]int64, error) {
	sizes := make(map[string]int64)
	for _, entry := range tree.Entries {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}

		switch entry.Mode {
		case filemode.Dir:
			subTree, err := gitea.BatchGetTree(bw, br, entry.Hash.String())
			if err != nil {
				return nil, err
			}
			subTreeSizes, err := batchAnalyzeTree(ctx, bw, br, subTree)
			if err != nil {
				return nil, err
			}
			for name, size := range subTreeSizes {
				sizes[name] += size
			}
		case filemode.Symlink, filemode.Submodule:
			// skip symlink/submodule
		default:
			_, err := bw.Write([]byte(entry.Hash.String() + "\n"))
			if err != nil {
				return nil, err
			}
			_, _, size, err := gitea.ReadBatchLine(br)
			if err != nil {
				return nil, err
			}
			// skip large file
			if size > bigFileSize {
				if err := gitea.DiscardFull(br, size+1); err != nil {
					return nil, err
				}
				continue
			}

			sizeToRead := size
			discard := int64(1)
			if size > fileSizeLimit {
				sizeToRead = fileSizeLimit
				discard = size - fileSizeLimit + 1
			}
			content, err := io.ReadAll(io.LimitReader(br, sizeToRead))
			if err != nil {
				return nil, err
			}
			if err := gitea.DiscardFull(br, discard); err != nil {
				return nil, err
			}

			language, noskip := analyzeLanguage(entry.Name, content)
			if noskip {
				sizes[language] += size
			}
		}
	}
	return sizes, nil
}

func analyzeLanguage(fileName string, content []byte) (string, bool) {
	// skip generated file
	// TODO: follow gitattributes, lazyily read content (filter by filename first)
	if enry.IsGenerated(fileName, content) || enry.IsBinary(content) || strings.HasSuffix(fileName, "bun.lock") {
		return "", false
	}

	language := func(fileName string, content []byte) string {
		language, ok := enry.GetLanguageByExtension(fileName)
		if ok {
			return language
		}
		language, ok = enry.GetLanguageByFilename(fileName)
		if ok {
			return language
		}
		if len(content) == 0 {
			return enry.OtherLanguage
		}
		return enry.GetLanguage(fileName, content)
	}(fileName, content)
	if group := enry.GetLanguageGroup(language); group != "" {
		language = group
	}

	langType := enry.GetLanguageType(language)
	if langType != enry.Programming && langType != enry.Markup && langType != enry.Unknown {
		return "", false
	}
	return language, true
}
