package timeline

import (
	"bytes"
	"io/fs"
	"log/slog"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/adrg/frontmatter"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
)

type postMeta struct {
	Slug     string `yaml:"slug"`
	Title    string `yaml:"title"`
	Subtitle string `yaml:"subtitle"`
	Date     string `yaml:"date"`
	Draft    bool   `yaml:"draft"`
}

type Timeline struct {
	oauth       *oauth.OAuth
	db          *db.DB
	config      *config.Config
	pages       *pages.Pages
	logger      *slog.Logger
	recentPosts []pages.BlogPost
}

func New(
	oauth *oauth.OAuth,
	db *db.DB,
	config *config.Config,
	pages *pages.Pages,
	logger *slog.Logger,
	postsDir string,
) *Timeline {
	t := &Timeline{
		oauth:  oauth,
		db:     db,
		config: config,
		pages:  pages,
		logger: logger,
	}
	t.recentPosts = loadRecentPosts(postsDir, logger)
	return t
}

func (t *Timeline) Router() http.Handler {
	r := chi.NewRouter()
	r.Get("/", t.HomeOrTimeline)
	r.Get("/home", t.Home)
	r.Get("/timeline", t.Timeline)
	return r
}

func loadRecentPosts(postsDir string, logger *slog.Logger) []pages.BlogPost {
	fsys := os.DirFS(postsDir)
	entries, err := fs.ReadDir(fsys, ".")
	if err != nil {
		logger.Warn("failed to read blog posts dir", "dir", postsDir, "err", err)
		return nil
	}

	var posts []postMeta
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".md") {
			continue
		}
		data, err := fs.ReadFile(fsys, entry.Name())
		if err != nil {
			continue
		}
		var meta postMeta
		if _, err := frontmatter.Parse(bytes.NewReader(data), &meta); err != nil {
			continue
		}
		if meta.Draft {
			continue
		}
		posts = append(posts, meta)
	}

	// sort newest-first by date string (format "2006-01-02" sorts lexicographically)
	for i := 1; i < len(posts); i++ {
		for j := i; j > 0 && posts[j].Date > posts[j-1].Date; j-- {
			posts[j], posts[j-1] = posts[j-1], posts[j]
		}
	}

	if len(posts) > 3 {
		posts = posts[:3]
	}

	result := make([]pages.BlogPost, len(posts))
	for i, p := range posts {
		t, _ := time.Parse("2006-01-02", p.Date)
		result[i] = pages.BlogPost{Slug: p.Slug, Title: p.Title, Subtitle: p.Subtitle, Date: t}
	}
	return result
}
