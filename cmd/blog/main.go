package main

import (
	"context"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"

	"tangled.org/core/appview/config"
	"tangled.org/core/appview/pages"
	"tangled.org/core/blog"
	"tangled.org/core/idresolver"
	tlog "tangled.org/core/log"
)

const (
	postsDir     = "blog/posts"
	templatesDir = "blog/templates"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: blog <build|serve> [flags]")
		os.Exit(1)
	}

	ctx := context.Background()
	logger := tlog.New("blog")

	switch os.Args[1] {
	case "build":
		if err := runBuild(ctx, logger); err != nil {
			logger.Error("build failed", "err", err)
			os.Exit(1)
		}
	case "serve":
		addr := "0.0.0.0:3001"
		if len(os.Args) >= 3 {
			addr = os.Args[2]
		}
		if err := runServe(ctx, logger, addr); err != nil {
			logger.Error("serve failed", "err", err)
			os.Exit(1)
		}
	default:
		fmt.Fprintf(os.Stderr, "unknown subcommand: %s\n", os.Args[1])
		os.Exit(1)
	}
}

func makePages(ctx context.Context, cfg *config.Config, logger *slog.Logger) (*pages.Pages, error) {
	resolver := idresolver.DefaultResolver(cfg.Plc.PLCURL)
	return pages.NewPages(cfg, resolver, nil, logger), nil
}

func runBuild(ctx context.Context, logger *slog.Logger) error {
	cfg, err := config.LoadConfig(ctx)
	if err != nil {
		cfg = &config.Config{}
	}

	p, err := makePages(ctx, cfg, logger)
	if err != nil {
		return fmt.Errorf("creating pages: %w", err)
	}

	posts, err := blog.Posts(postsDir)
	if err != nil {
		return fmt.Errorf("parsing posts: %w", err)
	}

	outDir := "build"
	if err := os.MkdirAll(outDir, 0755); err != nil {
		return err
	}

	// index
	if err := renderToFile(outDir, "index.html", func(w io.Writer) error {
		return blog.RenderIndex(p, templatesDir, posts, w)
	}); err != nil {
		return fmt.Errorf("rendering index: %w", err)
	}

	// posts — each at build/<slug>/index.html directly (no /blog/ prefix)
	for _, post := range posts {
		post := post
		postDir := filepath.Join(outDir, post.Meta.Slug)
		if err := os.MkdirAll(postDir, 0755); err != nil {
			return err
		}
		if err := renderToFile(postDir, "index.html", func(w io.Writer) error {
			return blog.RenderPost(p, templatesDir, post, w)
		}); err != nil {
			return fmt.Errorf("rendering post %s: %w", post.Meta.Slug, err)
		}
	}

	// atom feed — at build/feed.xml
	baseURL := "https://blog.tangled.org"
	atom, err := blog.AtomFeed(posts, baseURL)
	if err != nil {
		return fmt.Errorf("generating atom feed: %w", err)
	}
	if err := os.WriteFile(filepath.Join(outDir, "feed.xml"), []byte(atom), 0644); err != nil {
		return fmt.Errorf("writing feed: %w", err)
	}

	logger.Info("build complete", "dir", outDir)
	return nil
}

func runServe(ctx context.Context, logger *slog.Logger, addr string) error {
	cfg, err := config.LoadConfig(ctx)
	if err != nil {
		cfg = &config.Config{}
	}

	p, err := makePages(ctx, cfg, logger)
	if err != nil {
		return fmt.Errorf("creating pages: %w", err)
	}

	mux := http.NewServeMux()

	// index
	mux.HandleFunc("GET /", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		posts, err := blog.AllPosts(postsDir)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if err := blog.RenderIndex(p, templatesDir, posts, w); err != nil {
			logger.Error("render index", "err", err)
		}
	})

	// individual posts directly at /<slug>
	mux.HandleFunc("GET /{slug}", func(w http.ResponseWriter, r *http.Request) {
		slug := r.PathValue("slug")
		posts, err := blog.AllPosts(postsDir)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		for _, post := range posts {
			if post.Meta.Slug == slug {
				if err := blog.RenderPost(p, templatesDir, post, w); err != nil {
					logger.Error("render post", "err", err)
				}
				return
			}
		}
		http.NotFound(w, r)
	})

	// atom feed at /feed.xml
	mux.HandleFunc("GET /feed.xml", func(w http.ResponseWriter, r *http.Request) {
		posts, err := blog.Posts(postsDir)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		atom, err := blog.AtomFeed(posts, "https://blog.tangled.org")
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/atom+xml")
		fmt.Fprint(w, atom)
	})

	// appview static files (tw.css, fonts, icons, logos)
	mux.Handle("GET /static/", p.Static())

	logger.Info("serving", "addr", addr)
	return http.ListenAndServe(addr, mux)
}

func renderToFile(dir, name string, fn func(io.Writer) error) error {
	f, err := os.Create(filepath.Join(dir, name))
	if err != nil {
		return err
	}
	defer f.Close()
	return fn(f)
}
