package cursor

import (
	"context"
	"fmt"
	"strconv"

	"tangled.org/core/appview/cache"
)

const (
	cursorKey = "cursor:%s"
)

type RedisStore struct {
	rdb *cache.Cache
}

func NewRedisCursorStore(cache *cache.Cache) RedisStore {
	return RedisStore{
		rdb: cache,
	}
}

func (r *RedisStore) Set(key string, cursor int64) {
	k := fmt.Sprintf(cursorKey, key)
	r.rdb.Set(context.Background(), k, cursor, 0)
}

func (r *RedisStore) Get(key string) (cursor int64) {
	k := fmt.Sprintf(cursorKey, key)
	val, err := r.rdb.Get(context.Background(), k).Result()
	if err != nil {
		return 0
	}
	parsed, err := strconv.ParseInt(val, 10, 64)
	if err != nil {
		return 0
	}
	return parsed
}
