package blog

import (
	"bytes"
	"cmp"
	"html/template"
	"io"
	"io/fs"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/adrg/frontmatter"
	"github.com/gorilla/feeds"

	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/markup"
	textension "tangled.org/core/appview/pages/markup/extension"
)

type Author struct {
	Name   string `yaml:"name"`
	Email  string `yaml:"email"`
	Handle string `yaml:"handle"`
}

type PostMeta struct {
	Slug     string   `yaml:"slug"`
	Title    string   `yaml:"title"`
	Subtitle string   `yaml:"subtitle"`
	Date     string   `yaml:"date"`
	Authors  []Author `yaml:"authors"`
	Image    string   `yaml:"image"`
	Draft    bool     `yaml:"draft"`
}

type Post struct {
	Meta PostMeta
	Body template.HTML
}

func (p Post) ParsedDate() time.Time {
	t, _ := time.Parse("2006-01-02", p.Meta.Date)
	return t
}

type indexParams struct {
	LoggedInUser any
	Posts        []Post
	Featured     []Post
}

type postParams struct {
	LoggedInUser any
	Post         Post
}

// Posts parses and returns all non-draft posts sorted newest-first.
func Posts(postsDir string) ([]Post, error) {
	return parsePosts(postsDir, false)
}

// AllPosts parses and returns all posts including drafts, sorted newest-first.
func AllPosts(postsDir string) ([]Post, error) {
	return parsePosts(postsDir, true)
}

func parsePosts(postsDir string, includeDrafts bool) ([]Post, error) {
	fsys := os.DirFS(postsDir)

	entries, err := fs.ReadDir(fsys, ".")
	if err != nil {
		return nil, err
	}

	rctx := &markup.RenderContext{
		RendererType: markup.RendererTypeDefault,
		Sanitizer:    markup.NewSanitizer(),
	}
	var posts []Post
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".md") {
			continue
		}

		data, err := fs.ReadFile(fsys, entry.Name())
		if err != nil {
			return nil, err
		}

		var meta PostMeta
		rest, err := frontmatter.Parse(bytes.NewReader(data), &meta)
		if err != nil {
			return nil, err
		}

		if meta.Draft && !includeDrafts {
			continue
		}

		htmlStr := rctx.RenderMarkdownWith(string(rest), markup.NewMarkdownWith("", textension.Dashes))
		sanitized := rctx.SanitizeDefault(htmlStr)

		posts = append(posts, Post{
			Meta: meta,
			Body: template.HTML(sanitized),
		})
	}

	slices.SortFunc(posts, func(a, b Post) int {
		return cmp.Compare(b.Meta.Date, a.Meta.Date)
	})

	return posts, nil
}

func AtomFeed(posts []Post, baseURL string) (string, error) {
	feed := &feeds.Feed{
		Title:   "the tangled blog",
		Link:    &feeds.Link{Href: baseURL},
		Author:  &feeds.Author{Name: "Tangled"},
		Created: time.Now(),
	}

	for _, p := range posts {
		postURL := strings.TrimRight(baseURL, "/") + "/" + p.Meta.Slug

		var authorName string
		for i, a := range p.Meta.Authors {
			if i > 0 {
				authorName += " & "
			}
			authorName += a.Name
		}

		feed.Items = append(feed.Items, &feeds.Item{
			Title:       p.Meta.Title,
			Link:        &feeds.Link{Href: postURL},
			Description: p.Meta.Subtitle,
			Author:      &feeds.Author{Name: authorName},
			Created:     p.ParsedDate(),
		})
	}

	return feed.ToAtom()
}

// RenderIndex renders the blog index page to w.
func RenderIndex(p *pages.Pages, templatesDir string, posts []Post, w io.Writer) error {
	tpl, err := p.ParseWith(os.DirFS(templatesDir), "index.html")
	if err != nil {
		return err
	}
	var featured []Post
	for _, post := range posts {
		if post.Meta.Image != "" {
			featured = append(featured, post)
			if len(featured) == 3 {
				break
			}
		}
	}
	return tpl.ExecuteTemplate(w, "layouts/base", indexParams{Posts: posts, Featured: featured})
}

// RenderPost renders a single blog post page to w.
func RenderPost(p *pages.Pages, templatesDir string, post Post, w io.Writer) error {
	tpl, err := p.ParseWith(os.DirFS(templatesDir), "post.html")
	if err != nil {
		return err
	}
	return tpl.ExecuteTemplate(w, "layouts/base", postParams{Post: post})
}
