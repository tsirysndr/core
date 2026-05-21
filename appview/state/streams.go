package state

import (
	"context"

	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/config"
	ec "tangled.org/core/eventconsumer"
	"tangled.org/core/eventconsumer/cursor"
	"tangled.org/core/log"
)

func bootstrapStream(
	ctx context.Context,
	name string,
	kind ec.Kind,
	hosts []string,
	redisAddr string,
	streamCfg config.ConsumerConfig,
	dev bool,
	processFn ec.ProcessFunc,
) *ec.Consumer {
	logger := log.SubLogger(log.FromContext(ctx), name)

	redisCache := cache.New(redisAddr)
	cursorStore := cursor.NewRedisCursorStore(redisCache)

	srcs := make(map[ec.Source]struct{}, len(hosts))
	for _, h := range hosts {
		src := ec.Source{Kind: kind, Host: h}
		migrateLegacyCursor(&cursorStore, src)
		srcs[src] = struct{}{}
	}

	return ec.NewConsumer(ec.ConsumerConfig{
		Sources:           srcs,
		ProcessFunc:       processFn,
		RetryInterval:     streamCfg.RetryInterval,
		MaxRetryInterval:  streamCfg.MaxRetryInterval,
		ConnectionTimeout: streamCfg.ConnectionTimeout,
		WorkerCount:       streamCfg.WorkerCount,
		QueueSize:         streamCfg.QueueSize,
		Logger:            logger,
		URLFunc:           ec.DefaultURL(dev),
		CursorStore:       &cursorStore,
	})
}

func migrateLegacyCursor(store cursor.Store, src ec.Source) {
	if store.Get(src.Key()) != 0 {
		return
	}
	if legacy := store.Get(src.Host); legacy != 0 {
		store.Set(src.Key(), legacy)
	}
}
