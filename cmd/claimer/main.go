package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"os"
	"strings"

	appviewdb "tangled.org/core/appview/db"
	tlog "tangled.org/core/log"

	_ "github.com/mattn/go-sqlite3"
)

var (
	dbPath  = flag.String("db", "appview.db", "path to the appview SQLite database")
	pdsHost = flag.String("pds", "https://tngl.sh", "PDS host URL")
	dryRun  = flag.Bool("dry-run", false, "print what would be done without writing to the database")
	verbose = flag.Bool("v", false, "log pagination progress")
)

type repo struct {
	DID  string `json:"did"`
	Head string `json:"head"`
}

type listReposResponse struct {
	Cursor string `json:"cursor"`
	Repos  []repo `json:"repos"`
}

type describeRepoResponse struct {
	Handle string `json:"handle"`
	DID    string `json:"did"`
}

func main() {
	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage: claimer [flags]\n\n")
		fmt.Fprintf(os.Stderr, "Backfills domain_claims for all existing users on the tngl.sh PDS.\n")
		fmt.Fprintf(os.Stderr, "DIDs are fetched via com.atproto.sync.listRepos, handles are resolved\n")
		fmt.Fprintf(os.Stderr, "via com.atproto.repo.describeRepo (no DNS verification required).\n\n")
		fmt.Fprintf(os.Stderr, "Flags:\n")
		flag.PrintDefaults()
	}
	flag.Parse()

	ctx := context.Background()
	logger := tlog.New("claimer")
	ctx = tlog.IntoContext(ctx, logger)

	pdsDomain, err := domainFromURL(*pdsHost)
	if err != nil {
		logger.Error("invalid pds host", "host", *pdsHost, "error", err)
		os.Exit(1)
	}

	logger.Info("starting claimer",
		"pds", *pdsHost,
		"pds_domain", pdsDomain,
		"db", *dbPath,
		"dry_run", *dryRun,
	)

	database, err := appviewdb.Make(ctx, *dbPath)
	if err != nil {
		logger.Error("failed to open database", "error", err)
		os.Exit(1)
	}
	defer database.Close()

	dids, err := fetchAllDIDs(ctx, *pdsHost, logger)
	if err != nil {
		logger.Error("failed to fetch repos from PDS", "error", err)
		os.Exit(1)
	}

	logger.Info("fetched repos", "count", len(dids))

	suffix := "." + pdsDomain

	var (
		created  int
		skipped  int
		errCount int
	)

	for _, did := range dids {
		handle, err := describeRepo(ctx, *pdsHost, did)
		if err != nil {
			logger.Error("failed to describe repo", "did", did, "error", err)
			errCount++
			continue
		}

		if !strings.HasSuffix(handle, suffix) {
			logger.Info("skipping account: handle does not match pds domain",
				"did", did, "handle", handle, "suffix", suffix)
			skipped++
			continue
		}

		// The handle itself is the claim domain: <username>.<pds-domain>
		claimDomain := handle

		if *dryRun {
			logger.Info("[dry-run] would claim domain",
				"did", did,
				"handle", handle,
				"domain", claimDomain,
			)
			created++
			continue
		}

		if err := appviewdb.ClaimDomain(database, did, claimDomain); err != nil {
			switch {
			case errors.Is(err, appviewdb.ErrDomainTaken):
				logger.Warn("skipping: domain already claimed by another user",
					"did", did, "domain", claimDomain)
				skipped++
			case errors.Is(err, appviewdb.ErrAlreadyClaimed):
				logger.Warn("skipping: DID already has an active claim",
					"did", did, "domain", claimDomain)
				skipped++
			case errors.Is(err, appviewdb.ErrDomainCooldown):
				logger.Warn("skipping: domain is in 30-day cooldown",
					"did", did, "domain", claimDomain)
				skipped++
			default:
				logger.Error("failed to claim domain",
					"did", did, "domain", claimDomain, "error", err)
				errCount++
			}
			continue
		}

		logger.Info("claimed domain", "did", did, "domain", claimDomain)
		created++
	}

	logger.Info("claimer finished",
		slog.Group("results",
			"created", created,
			"skipped", skipped,
			"errors", errCount,
		),
	)

	if errCount > 0 {
		os.Exit(1)
	}
}

// fetchAllDIDs pages through com.atproto.sync.listRepos (public, no auth)
// and returns every DID hosted on the PDS.
func fetchAllDIDs(ctx context.Context, pdsHost string, logger *slog.Logger) ([]string, error) {
	var all []string
	cursor := ""

	for {
		batch, nextCursor, err := listRepos(ctx, pdsHost, cursor)
		if err != nil {
			return nil, err
		}

		for _, r := range batch {
			all = append(all, r.DID)
		}

		if *verbose {
			logger.Info("fetched page", "count", len(batch), "total", len(all), "next_cursor", nextCursor)
		}

		if nextCursor == "" || nextCursor == cursor {
			break
		}
		cursor = nextCursor
	}

	return all, nil
}

// listRepos fetches one page of com.atproto.sync.listRepos.
func listRepos(ctx context.Context, pdsHost, cursor string) ([]repo, string, error) {
	params := url.Values{}
	params.Set("limit", "100")
	if cursor != "" {
		params.Set("cursor", cursor)
	}

	endpoint := pdsHost + "/xrpc/com.atproto.sync.listRepos?" + params.Encode()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return nil, "", fmt.Errorf("building listRepos request: %w", err)
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, "", fmt.Errorf("executing listRepos request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var errBody struct {
			Error   string `json:"error"`
			Message string `json:"message"`
		}
		json.NewDecoder(resp.Body).Decode(&errBody)
		return nil, "", fmt.Errorf("PDS returned %d: %s — %s", resp.StatusCode, errBody.Error, errBody.Message)
	}

	var result listReposResponse
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, "", fmt.Errorf("decoding listRepos response: %w", err)
	}

	return result.Repos, result.Cursor, nil
}

// describeRepo calls com.atproto.repo.describeRepo on the PDS (public, no auth)
// and returns the handle for the given DID. This avoids DNS-based verification
// so accounts with broken handles still resolve correctly.
func describeRepo(ctx context.Context, pdsHost, did string) (string, error) {
	params := url.Values{}
	params.Set("repo", did)

	endpoint := pdsHost + "/xrpc/com.atproto.repo.describeRepo?" + params.Encode()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return "", fmt.Errorf("building describeRepo request: %w", err)
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("executing describeRepo request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var errBody struct {
			Error   string `json:"error"`
			Message string `json:"message"`
		}
		json.NewDecoder(resp.Body).Decode(&errBody)
		return "", fmt.Errorf("PDS returned %d: %s — %s", resp.StatusCode, errBody.Error, errBody.Message)
	}

	var result describeRepoResponse
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return "", fmt.Errorf("decoding describeRepo response: %w", err)
	}

	return result.Handle, nil
}

// domainFromURL strips the scheme from a URL and returns the bare hostname.
func domainFromURL(rawURL string) (string, error) {
	if !strings.Contains(rawURL, "://") {
		return rawURL, nil
	}
	u, err := url.Parse(rawURL)
	if err != nil {
		return "", err
	}
	host := u.Hostname()
	if host == "" {
		return "", fmt.Errorf("no hostname in URL %q", rawURL)
	}
	return host, nil
}
