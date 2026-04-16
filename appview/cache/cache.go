package cache

import (
	"context"
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/redis/go-redis/v9"
	"tangled.org/core/appview/db"
)

const (
	PreferredHandleByDid    = "preferred_handle:did:%s"
	PreferredHandleByHandle = "preferred_handle:handle:%s"
	PreferredHandleTTL      = 24 * time.Hour
)

type Cache struct {
	*redis.Client
}

func New(addr string) *Cache {
	rdb := redis.NewClient(&redis.Options{
		Addr: addr,
	})
	return &Cache{rdb}
}

func LookupPreferredHandle(ctx context.Context, rdb *Cache, e db.Execer, did string) string {
	if rdb != nil {
		if h, err := rdb.Get(ctx, fmt.Sprintf(PreferredHandleByDid, did)).Result(); err == nil {
			return h
		}
	}

	var handle string
	if h, err := db.GetPreferredHandle(e, did); err == nil {
		handle = string(h)
	}

	if rdb != nil {
		pipe := rdb.Pipeline()
		pipe.Set(ctx, fmt.Sprintf(PreferredHandleByDid, did), handle, PreferredHandleTTL)
		if handle != "" {
			pipe.Set(ctx, fmt.Sprintf(PreferredHandleByHandle, handle), did, PreferredHandleTTL)
		}
		pipe.Exec(ctx)
	}

	return handle
}

func LookupDidByPreferredHandle(ctx context.Context, rdb *Cache, e db.Execer, handle syntax.Handle) string {
	handleStr := string(handle)
	if rdb != nil {
		if d, err := rdb.Get(ctx, fmt.Sprintf(PreferredHandleByHandle, handleStr)).Result(); err == nil {
			return d
		}
	}

	var did string
	if d, err := db.GetDidByPreferredHandle(e, handle); err == nil {
		did = string(d)
	}

	if rdb != nil {
		pipe := rdb.Pipeline()
		pipe.Set(ctx, fmt.Sprintf(PreferredHandleByHandle, handleStr), did, PreferredHandleTTL)
		if did != "" {
			pipe.Set(ctx, fmt.Sprintf(PreferredHandleByDid, did), handleStr, PreferredHandleTTL)
		}
		pipe.Exec(ctx)
	}

	return did
}
