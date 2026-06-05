package knotacl

import (
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"slices"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
)

const (
	listPageLimit   = 1000
	maxListPages    = 256
	requestTimeout  = 5 * time.Second
	listDrainBudget = 30 * time.Second
)

type Client struct {
	dev    bool
	http   *http.Client
	logger *slog.Logger
}

func NewClient(dev bool, logger *slog.Logger) *Client {
	return &Client{dev: dev, http: &http.Client{Timeout: requestTimeout}, logger: logger}
}

func (c *Client) xrpcClient(host string) *indigoxrpc.Client {
	scheme := "https"
	if c.dev {
		scheme = "http"
	}
	return &indigoxrpc.Client{
		Host:   fmt.Sprintf("%s://%s", scheme, host),
		Client: c.http,
	}
}

func (c *Client) GetKnotMembers(ctx context.Context, host string) ([]string, error) {
	ctx, cancel := context.WithTimeout(ctx, listDrainBudget)
	defer cancel()

	xc := c.xrpcClient(host)
	subjects, truncated, err := drainList(
		"",
		make(map[string]struct{}),
		func(cursor string) ([]*tangled.KnotListMembers_ListItem, *string, error) {
			out, err := tangled.KnotListMembers(ctx, xc, cursor, listPageLimit, "", host)
			if err != nil {
				return nil, nil, err
			}
			return out.Items, out.Cursor, nil
		},
		func(i *tangled.KnotListMembers_ListItem) string { return i.Subject },
	)
	if err != nil {
		return nil, err
	}
	if truncated {
		c.logger.Warn("knot member list truncated before draining all pages", "host", host, "limit", maxListPages)
	}
	return dedup(subjects), nil
}

func (c *Client) GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error) {
	ctx, cancel := context.WithTimeout(ctx, listDrainBudget)
	defer cancel()

	xc := c.xrpcClient(host)
	subjects, truncated, err := drainList(
		"",
		make(map[string]struct{}),
		func(cursor string) ([]*tangled.RepoListCollaborators_ListItem, *string, error) {
			out, err := tangled.RepoListCollaborators(ctx, xc, cursor, listPageLimit, "", repoDid)
			if err != nil {
				return nil, nil, err
			}
			return out.Items, out.Cursor, nil
		},
		func(i *tangled.RepoListCollaborators_ListItem) string { return i.Subject },
	)
	if err != nil {
		return nil, err
	}
	if truncated {
		c.logger.Warn("repo collaborator list truncated before draining all pages", "host", host, "repoDid", repoDid, "limit", maxListPages)
	}
	return dedup(subjects), nil
}

func drainList[T any](
	cursor string,
	seen map[string]struct{},
	page func(cursor string) ([]*T, *string, error),
	subject func(*T) string,
) (subjects []string, truncated bool, err error) {
	if len(seen) >= maxListPages {
		return nil, true, nil
	}
	if _, repeated := seen[cursor]; repeated {
		return nil, true, nil
	}
	seen[cursor] = struct{}{}
	items, next, err := page(cursor)
	if err != nil {
		return nil, false, err
	}
	subjects = mapSlice(items, subject)
	if len(items) == 0 || next == nil || *next == "" {
		return subjects, false, nil
	}
	rest, truncated, err := drainList(*next, seen, page, subject)
	if err != nil {
		return nil, false, err
	}
	return append(subjects, rest...), truncated, nil
}

func dedup(subjects []string) []string {
	slices.Sort(subjects)
	return slices.Compact(subjects)
}

func mapSlice[T, U any](items []T, f func(T) U) []U {
	out := make([]U, len(items))
	for i, it := range items {
		out[i] = f(it)
	}
	return out
}
