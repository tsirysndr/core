package pulls

import (
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"log/slog"

	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	pulls_indexer "tangled.org/core/appview/indexer/pulls"
	"tangled.org/core/appview/mentions"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/validator"
	"tangled.org/core/idresolver"
	"tangled.org/core/ogre"
	"tangled.org/core/rbac"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
)

const ApplicationGzip = "application/gzip"

type Pulls struct {
	oauth            *oauth.OAuth
	repoResolver     *reporesolver.RepoResolver
	pages            *pages.Pages
	idResolver       *idresolver.Resolver
	mentionsResolver *mentions.Resolver
	db               *db.DB
	config           *config.Config
	notifier         notify.Notifier
	enforcer         *rbac.Enforcer
	logger           *slog.Logger
	validator        *validator.Validator
	indexer          *pulls_indexer.Indexer
	ogreClient       *ogre.Client
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
	enforcer *rbac.Enforcer,
	validator *validator.Validator,
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
		enforcer:         enforcer,
		logger:           logger,
		validator:        validator,
		indexer:          indexer,
		ogreClient:       ogre.NewClient(config.Ogre.Host),
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
