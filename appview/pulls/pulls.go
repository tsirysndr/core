package pulls

import (
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"log/slog"
	"strings"
	"time"

	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	pulls_indexer "tangled.org/core/appview/indexer/pulls"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/mentions"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/idresolver"
	"tangled.org/core/ogre"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"

	"github.com/hashicorp/golang-lru/v2/expirable"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

const ApplicationGzip = "application/gzip"

const (
	diffCacheSize = 128
	diffCacheTTL  = 15 * time.Minute
)

type Pulls struct {
	oauth            *oauth.OAuth
	repoResolver     *reporesolver.RepoResolver
	pages            *pages.Pages
	idResolver       *idresolver.Resolver
	mentionsResolver *mentions.Resolver
	db               *db.DB
	config           *config.Config
	notifier         notify.Notifier
	acl              *knotacl.Service
	logger           *slog.Logger
	indexer          *pulls_indexer.Indexer
	ogreClient       *ogre.Client
	diffCache        *expirable.LRU[string, types.DiffRenderer]
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	pages *pages.Pages,
	resolver *idresolver.Resolver,
	mentionsResolver *mentions.Resolver,
	db *db.DB,
	config *config.Config,
	notifier notify.Notifier,
	acl *knotacl.Service,
	indexer *pulls_indexer.Indexer,
	logger *slog.Logger,
) *Pulls {
	return &Pulls{
		oauth:            oauth,
		repoResolver:     repoResolver,
		pages:            pages,
		idResolver:       resolver,
		mentionsResolver: mentionsResolver,
		db:               db,
		config:           config,
		notifier:         notifier,
		acl:              acl,
		logger:           logger,
		indexer:          indexer,
		ogreClient:       ogre.NewClient(config.Ogre.Host),
		diffCache:        expirable.NewLRU[string, types.DiffRenderer](diffCacheSize, nil, diffCacheTTL),
	}
}

func (s *Pulls) knotClient(host string) *indigoxrpc.Client {
	scheme := "https"
	if s.config.Core.Dev {
		scheme = "http"
	}
	return &indigoxrpc.Client{Host: fmt.Sprintf("%s://%s", scheme, host)}
}

func gz(s string) io.Reader {
	var b bytes.Buffer
	w := gzip.NewWriter(&b)
	w.Write([]byte(s))
	w.Close()
	return &b
}

func ptrPullState(s models.PullState) *models.PullState { return &s }

func validatePatch(patch *string) error {
	if patch == nil || *patch == "" {
		return fmt.Errorf("patch is empty")
	}

	// add newline if not present to diff style patches
	if !patchutil.IsFormatPatch(*patch) && !strings.HasSuffix(*patch, "\n") {
		*patch = *patch + "\n"
	}

	if err := patchutil.IsPatchValid(*patch); err != nil {
		return err
	}

	return nil
}
