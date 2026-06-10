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
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sourcegraph/zoekt"
)

func main() {
	reposPath := flag.String("repos", "REPOS", "path to repos list file (one DID per line)")
	server := flag.String("server", "http://localhost:6060", "indexserver base url")
	plc := flag.String("plc", "https://plc.directory", "atproto PLC directory url")
	concurrency := flag.Int("concurrency", 5, "number of repos to process in parallel")
	flag.Parse()

	data, err := os.ReadFile(*reposPath)
	if err != nil {
		log.Fatalf("reading %s: %v", *reposPath, err)
	}

	ctx := context.Background()
	dir := identity.BaseDirectory{PLCURL: *plc}

	var ok, fail atomic.Int64
	var wg sync.WaitGroup
	sem := make(chan struct{}, *concurrency)

	for i, line := range strings.Split(string(data), "\n") {
		did := strings.TrimSpace(line)
		if did == "" {
			continue
		}

		wg.Add(1)
		sem <- struct{}{}
		go func(i int, did string) {
			defer wg.Done()
			defer func() { <-sem }()

			knot, err := resolveKnot(ctx, &dir, did)
			if err != nil {
				log.Printf("line %d: %s: resolving knot: %v", i+1, did, err)
				fail.Add(1)
				return
			}

			branch, sha, err := resolveHead(knot, did)
			if err != nil {
				log.Printf("line %d: %s: resolving HEAD: %v", i+1, did, err)
				fail.Add(1)
				return
			}

			if err := enqueue(*server, did, branch, sha); err != nil {
				log.Printf("line %d: %s: enqueue: %v", i+1, did, err)
				fail.Add(1)
				return
			}
			log.Printf("line %d: %s: enqueued %s@%s (knot=%s)", i+1, did, branch, sha, knot)
			ok.Add(1)
		}(i, did)
	}

	wg.Wait()
	fmt.Printf("done: %d enqueued, %d failed\n", ok.Load(), fail.Load())
}

func resolveKnot(ctx context.Context, dir identity.Directory, did string) (string, error) {
	d, err := syntax.ParseDID(did)
	if err != nil {
		return "", err
	}
	ident, err := dir.LookupDID(ctx, d)
	if err != nil {
		return "", err
	}
	knot := ident.PDSEndpoint()
	if knot == "" {
		return "", fmt.Errorf("no PDS endpoint in DID document")
	}
	return knot, nil
}

func resolveHead(knot, did string) (branch, sha string, err error) {
	url := strings.TrimRight(knot, "/") + "/" + did
	out, err := exec.Command(
		"git",
		"-c", "http.sslVerify=false",
		"ls-remote", "--symref", url, "HEAD",
	).Output()
	if err != nil {
		return "", "", fmt.Errorf("git ls-remote --symref %s HEAD: %w", url, err)
	}
	for line := range strings.SplitSeq(string(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		switch {
		case fields[0] == "ref:":
			branch = strings.TrimPrefix(fields[1], "refs/heads/")
		case fields[1] == "HEAD":
			sha = fields[0]
		}
	}
	if branch == "" || sha == "" {
		return "", "", fmt.Errorf("could not resolve HEAD (branch=%q sha=%q)", branch, sha)
	}
	return branch, sha, nil
}

func enqueue(server, did, branch, sha string) error {
	body, err := json.Marshal(map[string]any{
		"repo":     did,
		"branches": []zoekt.RepositoryBranch{{Name: branch, Version: sha}},
	})
	if err != nil {
		return err
	}

	resp, err := http.Post(strings.TrimRight(server, "/")+"/admin/enqueueIndex",
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
