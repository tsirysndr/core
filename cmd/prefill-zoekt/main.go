// prefill-zoekt bulk-enqueues zoekt index tasks
//
// It reads a REPOS file with one repo DID per line:
//
//	did:plc:repository
//
// For each DID it resolves the knot from the DID document, then resolves the
// remote HEAD branch + commit via `git ls-remote --symref`, and POSTs an
// enqueue request to the indexserver's /admin/enqueueIndex endpoint.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/samber/lo"
	"github.com/sourcegraph/zoekt"
	"tangled.org/core/repoident"
)

func main() {
	reposPath := flag.String("repos", "REPOS", "path to repos list file (one DID per line)")
	serverUrl := flag.String("server", "http://localhost:6060", "indexserver base url")
	plc := flag.String("plc", "https://plc.directory", "atproto PLC directory url")
	concurrency := flag.Int("concurrency", 5, "number of repos to process in parallel")
	allowHttp := flag.Bool("allow-http", false, "accept repo DIDs whose knot service endpoint is plaintext http, and skip TLS verification when reading HEAD")
	flag.Parse()

	server, err := url.Parse(*serverUrl)
	if err != nil {
		log.Fatalf("parsing -server %q: %v", *serverUrl, err)
	}
	if (server.Scheme != "http" && server.Scheme != "https") || server.Host == "" {
		log.Fatalf("-server %q must be an http or https URL with a host", *serverUrl)
	}

	data, err := os.ReadFile(*reposPath)
	if err != nil {
		log.Fatalf("reading %s: %v", *reposPath, err)
	}

	ctx := context.Background()
	dir := identity.BaseDirectory{PLCURL: *plc}

	var ok, fail atomic.Int64
	var wg sync.WaitGroup
	sem := make(chan struct{}, *concurrency)

	lo.ForEach(strings.Split(string(data), "\n"), func(line string, i int) {
		raw := strings.TrimSpace(line)
		if raw == "" {
			return
		}

		wg.Add(1)
		sem <- struct{}{}
		go func() {
			defer wg.Done()
			defer func() { <-sem }()

			head, knot, err := prefillRepo(ctx, &dir, server, raw, *allowHttp)
			if err != nil {
				log.Printf("line %d: %s: %v", i+1, raw, err)
				fail.Add(1)
				return
			}
			log.Printf("line %d: %s: enqueued %s@%s (knot=%s)", i+1, raw, head.Name, head.Version, knot)
			ok.Add(1)
		}()
	})

	wg.Wait()
	fmt.Printf("done: %d enqueued, %d failed\n", ok.Load(), fail.Load())
}

func prefillRepo(ctx context.Context, dir identity.Directory, server *url.URL, raw string, allowHTTP bool) (zoekt.RepositoryBranch, repoident.KnotURL, error) {
	var knot repoident.KnotURL

	repoDid, err := repoident.NewRepoDid(raw)
	if err != nil {
		return zoekt.RepositoryBranch{}, knot, err
	}

	ident, err := dir.LookupDID(ctx, syntax.DID(repoDid))
	if err != nil {
		return zoekt.RepositoryBranch{}, knot, fmt.Errorf("resolving repo DID: %w", err)
	}

	knot, err = repoident.KnotURLFromIdentity(ident, repoident.SchemeFor(allowHTTP))
	if err != nil {
		return zoekt.RepositoryBranch{}, knot, fmt.Errorf("resolving knot: %w", err)
	}

	head, err := resolveHead(knot, repoDid, allowHTTP)
	if err != nil {
		return head, knot, fmt.Errorf("resolving HEAD: %w", err)
	}

	if err := enqueue(server, repoDid, head); err != nil {
		return head, knot, fmt.Errorf("enqueue: %w", err)
	}
	return head, knot, nil
}

func resolveHead(knot repoident.KnotURL, repoDid repoident.RepoDid, allowHTTP bool) (zoekt.RepositoryBranch, error) {
	remote := knot.JoinPath(repoDid.String())
	args := append(
		lo.Ternary(allowHTTP, []string{"-c", "http.sslVerify=false"}, nil),
		"ls-remote", "--symref", remote, "HEAD",
	)
	out, err := exec.Command("git", args...).Output()
	if err != nil {
		return zoekt.RepositoryBranch{}, fmt.Errorf("git ls-remote --symref %s HEAD: %w", remote, err)
	}
	head := lo.Reduce(
		strings.Split(string(out), "\n"),
		func(head zoekt.RepositoryBranch, line string, _ int) zoekt.RepositoryBranch {
			fields := strings.Fields(line)
			if len(fields) < 2 {
				return head
			}
			switch {
			case fields[0] == "ref:":
				head.Name = strings.TrimPrefix(fields[1], "refs/heads/")
			case fields[1] == "HEAD":
				head.Version = fields[0]
			}
			return head
		},
		zoekt.RepositoryBranch{},
	)
	if head.Name == "" || head.Version == "" {
		return zoekt.RepositoryBranch{}, fmt.Errorf("couldn't resolve HEAD (branch=%q sha=%q)", head.Name, head.Version)
	}
	return head, nil
}

func enqueue(server *url.URL, repoDid repoident.RepoDid, head zoekt.RepositoryBranch) error {
	body, err := json.Marshal(map[string]any{
		"repo":     repoDid.String(),
		"branches": []zoekt.RepositoryBranch{head},
	})
	if err != nil {
		return err
	}

	resp, err := http.Post(server.JoinPath("admin", "enqueueIndex").String(),
		"application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	log.Println("status", resp.StatusCode)

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("status %d", resp.StatusCode)
	}
	return nil
}
