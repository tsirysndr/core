package compat113

import (
	"context"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
)

const versionProbeTimeout = 5 * time.Second

func KnotSupports114(ctx context.Context, host string, dev bool) bool {
	scheme := "https"
	if dev {
		scheme = "http"
	}
	client := &indigoxrpc.Client{
		Host:   fmt.Sprintf("%s://%s", scheme, host),
		Client: &http.Client{Timeout: versionProbeTimeout},
	}

	ctx, cancel := context.WithTimeout(ctx, versionProbeTimeout)
	defer cancel()

	resp, err := tangled.KnotVersion(ctx, client)
	if err != nil || resp == nil {
		return true
	}
	return atLeast114(resp.Version)
}

func atLeast114(v string) bool {
	v = strings.TrimSpace(v)
	v = strings.TrimPrefix(v, "v")
	if strings.HasPrefix(v, "(devel)") {
		return true
	}
	if v == "" {
		return false
	}
	parts := strings.SplitN(v, ".", 3)
	if len(parts) < 2 {
		return false
	}
	major, err := strconv.Atoi(parts[0])
	if err != nil {
		return false
	}
	minorRaw := strings.SplitN(parts[1], "-", 2)[0]
	minor, err := strconv.Atoi(minorRaw)
	if err != nil {
		return false
	}
	return major > 1 || (major == 1 && minor >= 14)
}
